//! Windows サービスの入口。マクロが FFI 用のコードを生成するため、このファイルだけ許可する。
//! (他のファイルは `deny` のままで、テストで他に増えていないことを確かめている。)
#![allow(unsafe_code)]

use std::ffi::OsString;

use windows_service::{define_windows_service, service_dispatcher};

use crate::winsvc::service_main;

define_windows_service!(ffi_service_main, service_main_with_args);

fn service_main_with_args(_args: Vec<OsString>) {
    service_main();
}

/// SCM から起動されたときに呼ぶ。サービスが止まるまで戻らない。
pub fn start(name: &str) -> windows_service::Result<()> {
    service_dispatcher::start(name, ffi_service_main)
}
