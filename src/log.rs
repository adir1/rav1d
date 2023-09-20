use crate::src::internal::Dav1dContext;
use crate::stderr;

extern "C" {
    fn fprintf(_: *mut libc::FILE, _: *const libc::c_char, _: ...) -> libc::c_int;
    fn vfprintf(_: *mut libc::FILE, _: *const libc::c_char, _: ::core::ffi::VaList) -> libc::c_int;
}

#[cold]
pub unsafe extern "C" fn dav1d_log_default_callback(
    _cookie: *mut libc::c_void,
    format: *const libc::c_char,
    mut ap: ::core::ffi::VaList,
) {
    vfprintf(stderr, format, ap.as_va_list());
}

#[no_mangle]
#[cold]
pub unsafe extern "C" fn dav1d_log(c: *mut Dav1dContext, format: *const libc::c_char, args: ...) {
    if c.is_null() {
        fprintf(
            stderr,
            b"Input validation check '%s' failed in %s!\n\0" as *const u8 as *const libc::c_char,
            b"c != ((void*)0)\0" as *const u8 as *const libc::c_char,
            (*::core::mem::transmute::<&[u8; 10], &[libc::c_char; 10]>(b"dav1d_log\0")).as_ptr(),
        );
        return;
    }
    if ((*c).logger.callback).is_none() {
        return;
    }
    let mut ap: ::core::ffi::VaListImpl;
    ap = args.clone();
    ((*c).logger.callback).expect("non-null function pointer")(
        (*c).logger.cookie,
        format,
        ap.as_va_list(),
    );
}
