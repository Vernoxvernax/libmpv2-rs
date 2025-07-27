use super::*;

use std::alloc::{self, Layout};
use std::mem;
use std::os::raw as ctype;
use std::panic;
use std::panic::RefUnwindSafe;
use std::slice;

/// Return a persistent `T` that is passed to all other `Stream*` functions, panic on errors.
pub type StreamOpen<T, U> = fn(&mut U, &str) -> T;
/// Do any necessary cleanup.
pub type StreamClose<T> = fn(Box<T>);
/// Seek to the given offset. Return the new offset, or either `MpvError::Generic` if seeking
/// failed or panic.
pub type StreamSeek<T> = fn(&mut T, i64) -> i64;
/// Target buffer with fixed capacity.
/// Return either the number of read bytes, `0` on EOF, or either `-1` or panic on error.
pub type StreamRead<T> = fn(&mut T, &mut [ctype::c_char]) -> i64;
/// Return the total size of the stream in bytes. Panic on error.
pub type StreamSize<T> = fn(&mut T) -> i64;

unsafe extern "C" fn open_wrapper<T, U>(
    user_data: *mut ctype::c_void,
    uri: *mut ctype::c_char,
    info: *mut libmpv2_sys::mpv_stream_cb_info,
) -> ctype::c_int
where
    T: RefUnwindSafe,
    U: RefUnwindSafe,
{
    let data = user_data as *mut ProtocolData<T, U>;

    unsafe {
        (*info).cookie = user_data;
        (*info).read_fn = Some(read_wrapper::<T, U>);
        (*info).seek_fn = Some(seek_wrapper::<T, U>);
        (*info).size_fn = Some(size_wrapper::<T, U>);
        (*info).close_fn = Some(close_wrapper::<T, U>);
    }

    let ret = panic::catch_unwind(|| unsafe {
        let uri = mpv_cstr_to_str!(uri as *const _).unwrap();
        ptr::write(
            (*data).cookie,
            ((*data).open_fn)(&mut (*data).user_data, uri),
        );
    });

    if ret.is_ok() {
        0
    } else {
        mpv_error::Generic as _
    }
}

unsafe extern "C" fn read_wrapper<T, U>(
    cookie: *mut ctype::c_void,
    buf: *mut ctype::c_char,
    nbytes: u64,
) -> i64
where
    T: RefUnwindSafe,
    U: RefUnwindSafe,
{
    let data = cookie as *mut ProtocolData<T, U>;

    let ret = panic::catch_unwind(|| unsafe {
        let slice = slice::from_raw_parts_mut(buf, nbytes as _);
        ((*data).read_fn)(&mut *(*data).cookie, slice)
    });
    if let Ok(ret) = ret { ret } else { -1 }
}

unsafe extern "C" fn seek_wrapper<T, U>(cookie: *mut ctype::c_void, offset: i64) -> i64
where
    T: RefUnwindSafe,
    U: RefUnwindSafe,
{
    let data = cookie as *mut ProtocolData<T, U>;

    if unsafe { (*data).seek_fn.is_none() } {
        return mpv_error::Unsupported as _;
    }

    let ret = panic::catch_unwind(|| unsafe {
        (*(*data).seek_fn.as_ref().unwrap())(&mut *(*data).cookie, offset)
    });
    if let Ok(ret) = ret {
        ret
    } else {
        mpv_error::Generic as _
    }
}

unsafe extern "C" fn size_wrapper<T, U>(cookie: *mut ctype::c_void) -> i64
where
    T: RefUnwindSafe,
    U: RefUnwindSafe,
{
    let data = cookie as *mut ProtocolData<T, U>;

    if unsafe { (*data).size_fn.is_none() } {
        return mpv_error::Unsupported as _;
    }

    let ret = panic::catch_unwind(|| unsafe {
        (*(*data).size_fn.as_ref().unwrap())(&mut *(*data).cookie)
    });
    if let Ok(ret) = ret {
        ret
    } else {
        mpv_error::Unsupported as _
    }
}

#[allow(unused_must_use)]
unsafe extern "C" fn close_wrapper<T, U>(cookie: *mut ctype::c_void)
where
    T: RefUnwindSafe,
    U: RefUnwindSafe,
{
    let data = unsafe { Box::from_raw(cookie as *mut ProtocolData<T, U>) };

    panic::catch_unwind(|| ((*data).close_fn)(unsafe { Box::from_raw((*data).cookie) }));
}

struct ProtocolData<T, U> {
    cookie: *mut T,
    user_data: U,

    open_fn: StreamOpen<T, U>,
    close_fn: StreamClose<T>,
    read_fn: StreamRead<T>,
    seek_fn: Option<StreamSeek<T>>,
    size_fn: Option<StreamSize<T>>,
}

/// `Protocol` holds all state used by a custom protocol.
pub struct Protocol<'parent, T: Sized + RefUnwindSafe, U: RefUnwindSafe> {
    mpv: &'parent Mpv,
    name: String,
    data: *mut ProtocolData<T, U>,
}

unsafe impl<'parent, T: RefUnwindSafe, U: RefUnwindSafe> Send for Protocol<'parent, T, U> {}
unsafe impl<'parent, T: RefUnwindSafe, U: RefUnwindSafe> Sync for Protocol<'parent, T, U> {}

impl<'parent, T: RefUnwindSafe, U: RefUnwindSafe> Protocol<'parent, T, U> {
    /// `name` is the prefix of the protocol, e.g. `name://path`.
    ///
    /// `user_data` is data that will be passed to `open_fn`.
    ///
    /// # Safety
    /// Do not call libmpv functions in any supplied function.
    /// All panics of the provided functions are catched and can be used as generic error returns.
    pub unsafe fn new(
        mpv: &Mpv,
        name: String,
        user_data: U,
        open_fn: StreamOpen<T, U>,
        close_fn: StreamClose<T>,
        read_fn: StreamRead<T>,
        seek_fn: Option<StreamSeek<T>>,
        size_fn: Option<StreamSize<T>>,
    ) -> Protocol<T, U> {
        let c_layout = Layout::from_size_align(mem::size_of::<T>(), mem::align_of::<T>()).unwrap();
        let cookie = unsafe { alloc::alloc(c_layout) as *mut T };
        let data = Box::into_raw(Box::new(ProtocolData {
            cookie,
            user_data,

            open_fn,
            close_fn,
            read_fn,
            seek_fn,
            size_fn,
        }));

        Protocol { mpv, name, data }
    }

    /// This will register the `Protocol`, and invoke the given callbacks if an
    /// URI with the matching protocol prefix is opened.
    ///
    /// Will return `Err` if a `Protocol` with the same name is already
    /// registered
    pub fn register(&self) -> Result<()> {
        let name = CString::new(&self.name[..])?;
        unsafe {
            mpv_err(
                (),
                libmpv2_sys::mpv_stream_cb_add_ro(
                    self.mpv.ctx.as_ptr(),
                    name.as_ptr(),
                    self.data as *mut _,
                    Some(open_wrapper::<T, U>),
                ),
            )
        }
    }
}
