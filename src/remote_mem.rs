//! Can be used to read and write to memory of another process.
//!
//! While this is safe for the process using this module, it is very possibly not for the affected
//! process.
//!
//! Note: From the `process_vm_readv` man page: Permission  to  read  from  or  write to another
//! process is governed by a ptrace access mode PTRACE_MODE_AT‐TACH_REALCREDS check; see
//! `ptrace(2)`.
use libc::c_void;
use nix::sys::uio::{process_vm_readv, process_vm_writev, IoVec, RemoteIoVec};
use nix::unistd::Pid;
use std::mem::size_of;
use std::mem::MaybeUninit;

/// We assume the platforms memcopy (used in process_vm_read/write) copies chunks of data aligned
/// by <=ALG atomically. This is relevant for process_load/store.
///
/// The *rationale* is that all reasonable platforms rather introduce _one_ branch to check wether
/// byte-wise memaccess can be replaced by the platforms native access width, rather than e.g.
/// copying a u64 in a byte copy loop containing _eight_ conditionals. 
const ALG: usize = 8;

/// An Error Type.
#[derive(Debug)]
pub enum Error {
    /// process_vm_readv failed
    Rw(nix::Error),
    /// process_vm_readv read {} bytes when {} were expected
    ByteCount {
        /// ffs
        is: usize,
        /// nope
        should: usize,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Rw(e) => write!(f, "cannot read/write from remote process memory: {}", e),
            Error::ByteCount { is, should } => write!(
                f,
                "reading from remote process memory: {} bytes completed, {} bytes expected",
                is, should
            ),
        }
    }
}

/// # Safety
///
/// None. See safety chapter of `std::slice::from_raw_parts`.
pub unsafe fn any_as_bytes<T: Sized>(p: &T) -> &[u8] {
    std::slice::from_raw_parts((p as *const T) as *const u8, size_of::<T>())
}

/// read from a virtual addr of the hypervisor
pub fn process_load<T: Sized + Copy>(pid: Pid, addr: *const c_void) -> Result<T, Error> {
    let foo: T = process_read(pid, addr)?;
    log::trace!("load::foo_read = {:?}", unsafe { any_as_bytes(&foo) }); // 0x0100

    let len = size_of::<T>();
    assert!(len <= ALG);

    // TODO kind of safe, because we access at most 7 bytes (actually 6) more than we are allowed
    // to lol
    let offset = ALG - addr.align_offset(ALG); // alignment border <--offset--> addr <----> algn b.
    log::trace!("load offset {}", offset);
    let aligned = unsafe { addr.sub(offset) } as usize;
    //let addr = addr as usize;
    //let aligned = addr & (usize::MAX << 6); // 8byte aligned
    assert!(addr as usize + len <= aligned + ALG); // value must not extend beyond this 8b aligned space

    assert_eq!(size_of::<MaybeUninit::<T>>(), size_of::<T>());
    let mut t_mem = MaybeUninit::<T>::uninit();
    let t_slice = unsafe { std::slice::from_raw_parts_mut(t_mem.as_mut_ptr() as *mut u8, len) };
    //let read = process_read_bytes(pid, t_slice, addr)?;
    let data: [u8; ALG] = process_read(pid, aligned as *const c_void)?;
    log::trace!("load::read {:?}", data); // 0
    t_slice.copy_from_slice(&data[offset .. (offset+len)]);
    log::trace!("load = {:?}", t_slice); // 0
    let t: T = unsafe { t_mem.assume_init() };

    Ok(t)
}

/// read from a virtual addr of the hypervisor
pub fn process_read<T: Sized + Copy>(pid: Pid, addr: *const c_void) -> Result<T, Error> {
    let len = size_of::<T>();
    let mut t_mem = MaybeUninit::<T>::uninit();
    let t_slice = unsafe { std::slice::from_raw_parts_mut(t_mem.as_mut_ptr() as *mut u8, len) };
    let read = process_read_bytes(pid, t_slice, addr)?;
    if read != len {
        return Err(Error::ByteCount {
            is: read,
            should: len,
        });
    }
    let t: T = unsafe { t_mem.assume_init() };
    Ok(t)
}

/// read from a virtual addr of the hypervisor
pub fn process_read_bytes(pid: Pid, buf: &mut [u8], addr: *const c_void) -> Result<usize, Error> {
    let len = buf.len();
    let local_iovec = vec![IoVec::from_mut_slice(buf)];
    let remote_iovec = vec![RemoteIoVec {
        base: addr as usize,
        len,
    }];

    let f = process_vm_readv(pid, local_iovec.as_slice(), remote_iovec.as_slice())
        .map_err(Error::Rw)?;
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    Ok(f)
}

/// write to a virtual addr of the hypervisor
pub fn process_store<T: Sized + Copy>(pid: Pid, addr: *mut c_void, val: &T) -> Result<(), Error> {

    let len = size_of::<T>();
    assert!(len <= ALG); // Thats our limit. The hardware may support less.

    // TODO kind of safe, because we access at most 7 bytes (actually 6) more than we are allowed
    // to lol
    let offset = addr.align_offset(ALG);
    log::trace!("store offset {}", offset);
    let aligned = unsafe { addr.add(offset) } as usize;
    //let addr = addr as usize;
    //let aligned = addr & (usize::MAX << 6); // 8byte aligned
    //assert!(aligned + ALG >= addr + len); // value must not extend beyond this 8b aligned space

    let mut data: [u8; ALG] = process_read(pid, aligned as *const c_void)?;
    let val_b: &[u8] = unsafe { any_as_bytes(val) };
    //let data_slice = &mut data[offset .. (offset+len)];
    //data_slice.copy_from_slice(val_b);
    data[offset .. (offset+len)].copy_from_slice(val_b);
    process_write(pid, addr, &data)?;

    // TODO are we the only vmsh writing? that will depend on who is calling operations on the
    // queue. But vmsh owns the queue code so that should be fine. 

    Ok(())
}

/// write to a virtual addr of the hypervisor
pub fn process_write<T: Sized + Copy>(pid: Pid, addr: *mut c_void, val: &T) -> Result<(), Error> {
    let len = size_of::<T>();
    // safe, because we won't need t_bytes for long
    let t_bytes = unsafe { any_as_bytes(val) };
    let written = process_write_bytes(pid, addr, t_bytes)?;
    if written != len {
        return Err(Error::ByteCount {
            is: written,
            should: len,
        });
    }

    Ok(())
}

/// write to a virtual addr of the hypervisor TODO
pub fn process_write_bytes(pid: Pid, addr: *mut c_void, val: &[u8]) -> Result<usize, Error> {
    let len = val.len();
    let local_iovec = vec![IoVec::from_slice(val)];
    let remote_iovec = vec![RemoteIoVec {
        base: addr as usize,
        len,
    }];

    let f = process_vm_writev(pid, local_iovec.as_slice(), remote_iovec.as_slice())
        .map_err(Error::Rw)?;
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    Ok(f)
}
