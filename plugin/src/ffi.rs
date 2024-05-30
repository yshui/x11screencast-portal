use std::os::raw::c_void;

use libffi::{
    high::CType,
    low::CodePtr,
    raw::{self, ffi_closure},
};

// Converts the raw status type to a `Result`.
fn status_to_result(status: raw::ffi_status) -> Result<(), libffi::low::Error> {
    if status == raw::ffi_status_FFI_OK {
        Ok(())
    } else if status == raw::ffi_status_FFI_BAD_TYPEDEF {
        Err(libffi::low::Error::Typedef)
    }
    // If we don't recognize the status, that is an ABI error:
    else {
        Err(libffi::low::Error::Abi)
    }
}

struct ClosureData1<A, U, R> {
    callback: fn(&mut A, *mut U) -> R,
    userdata: U,
    cif:      libffi::low::ffi_cif,
    atypes:   [*mut libffi::low::ffi_type; 1],
}

pub struct Closure1<A, U, R> {
    closure: *mut ffi_closure,
    fnptr:   CodePtr,
    data:    *mut ClosureData1<A, U, R>,
}

impl<A, U, R> Closure1<A, U, R> {
    pub fn code_ptr(&self) -> *mut c_void { self.fnptr.0 }
}

impl<A, U, R> Drop for Closure1<A, U, R> {
    fn drop(&mut self) {
        unsafe {
            libffi::low::closure_free(self.closure);
            drop(Box::from_raw(self.data));
        }
    }
}

unsafe extern "C" fn closure1_trampoline<A, U, R: CType>(
    _: *mut libffi::raw::ffi_cif,
    ret_: *mut c_void,
    args: *mut *mut c_void,
    userdata_: *mut c_void,
) {
    let args = args as *mut *mut A;
    // Calling `callback` can free `userdata_`, so we can't keep mut reference to
    // it.
    let callback = (*(userdata_ as *mut ClosureData1<A, U, R>)).callback;
    let userdata = &mut (*(userdata_ as *mut ClosureData1<A, U, R>)).userdata as *mut U;
    let ret = &mut *(ret_ as *mut R::RetType);
    *ret = (callback)(&mut **args, userdata).into();
}

/// Create a new FFI closure that calls `callback` with `userdata` as the second
/// argument.
///
/// # Safety
///
/// - The returned function pointer must not be called after `closure` is
///   dropped.
/// - userdata must have been initialized before the returned function pointer
///   is called the first time.
pub unsafe fn make_ffi_closure1<U, A: CType, R: CType>(
    callback: fn(&mut A, *mut U) -> R,
    userdata: U,
) -> anyhow::Result<Closure1<A, U, R>>
where
    R::RetType: CType,
{
    let (closure, fnptr) = libffi::low::closure_alloc();
    let data = Box::leak(Box::new(ClosureData1 {
        callback,
        userdata,
        cif: Default::default(),
        atypes: [A::reify().into_middle().as_raw_ptr()],
    }));
    let rtype = <R::RetType as CType>::reify().into_middle().as_raw_ptr();
    libffi::low::prep_cif(
        &mut data.cif,
        libffi::low::ffi_abi_FFI_DEFAULT_ABI,
        1,
        rtype,
        data.atypes.as_mut_ptr(),
    )
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    status_to_result(libffi::raw::ffi_prep_closure_loc(
        closure,
        &mut data.cif,
        Some(closure1_trampoline::<A, U, R>),
        data as *mut _ as _,
        fnptr.0,
    ))
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(Closure1 { closure, fnptr, data })
}

pub struct Closure2<A, B, U, R> {
    closure: *mut ffi_closure,
    fnptr:   CodePtr,
    data:    *mut ClosureData2<A, B, U, R>,
}

impl<A, B, U, R> Closure2<A, B, U, R> {
    pub fn code_ptr(&self) -> *mut c_void { self.fnptr.0 }
}

struct ClosureData2<A, B, U, R> {
    callback: fn(&mut A, &mut B, *mut U) -> R,
    userdata: U,
    cif:      libffi::low::ffi_cif,
    atypes:   [*mut libffi::low::ffi_type; 2],
}

impl<A, B, U, R> Drop for Closure2<A, B, U, R> {
    fn drop(&mut self) {
        unsafe {
            libffi::low::closure_free(self.closure);
            drop(Box::from_raw(self.data));
        }
    }
}

unsafe extern "C" fn closure2_trampoline<A, B, U, R: CType>(
    _: *mut libffi::raw::ffi_cif,
    ret_: *mut c_void,
    args: *mut *mut c_void,
    userdata_: *mut c_void,
) {
    let [arg1, arg2] = *(args as *const [*mut c_void; 2]);
    let ret = ret_ as *mut R::RetType;
    // Calling `callback` can free `userdata_`, so we can't keep mut reference to
    // it.
    let callback = (*(userdata_ as *mut ClosureData2<A, B, U, R>)).callback;
    let userdata = &mut (*(userdata_ as *mut ClosureData2<A, B, U, R>)).userdata as *mut U;
    *ret = (callback)(&mut *(arg1 as *mut A), &mut *(arg2 as *mut B), userdata).into();
}

pub unsafe fn make_ffi_closure2<U, A: CType, B: CType, R: CType>(
    callback: fn(&mut A, &mut B, *mut U) -> R,
    userdata: U,
) -> anyhow::Result<Closure2<A, B, U, R>>
where
    R::RetType: CType,
{
    let (closure, fnptr) = libffi::low::closure_alloc();
    let data = Box::leak(Box::new(ClosureData2 {
        callback,
        userdata,
        cif: Default::default(),
        atypes: [A::reify().into_middle().as_raw_ptr(), B::reify().into_middle().as_raw_ptr()],
    }));
    let rtype = <R::RetType as CType>::reify().into_middle().as_raw_ptr();
    libffi::low::prep_cif(
        &mut data.cif,
        libffi::low::ffi_abi_FFI_DEFAULT_ABI,
        2,
        rtype,
        data.atypes.as_mut_ptr(),
    )
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    status_to_result(libffi::raw::ffi_prep_closure_loc(
        closure,
        &mut data.cif,
        Some(closure2_trampoline::<A, B, U, R>),
        data as *mut _ as _,
        fnptr.0,
    ))
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(Closure2 { closure, fnptr, data })
}
