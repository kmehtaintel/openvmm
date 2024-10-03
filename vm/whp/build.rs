// Copyright (C) Microsoft Corporation. All rights reserved.

fn main() {
    // WinHvPlatform isn't in the public SDK yet, so build a minimal import lib
    // for it.
    if std::env::var_os("TARGET").unwrap() == "aarch64-pc-windows-msvc" {
        win_import_lib::build_import_lib("WinHvPlatform").unwrap();
    }
}