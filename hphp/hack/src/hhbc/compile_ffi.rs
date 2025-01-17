// Copyright (c) 2019, Facebook, Inc.
// All rights reserved.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the "hack" directory of this source tree.

use decl_provider::NoDeclProvider;
use external_decl_provider::ExternalDeclProvider;
use hhbc_by_ref_hhas_program::HhasProgram;
use ocamlrep::{rc::RcOc, FromOcamlRep};
use ocamlrep_derive::FromOcamlRep;
use ocamlrep_ocamlpool::to_ocaml;
use oxidized::relative_path::RelativePath;
use parser_core_types::source_text::SourceText;

use anyhow::{anyhow, Result};
use serde_json::{map::Map, value::Value};
use std::io::Write;

use libc::{c_char, c_int};
use log::warn;

#[derive(Debug, FromOcamlRep)]
pub struct RustOutputConfig {
    include_header: bool,
    output_file: Option<String>,
}

pub struct OcamlStr<'content>(&'content [u8]);

impl<'content> AsRef<str> for OcamlStr<'content> {
    fn as_ref(&self) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.0) }
    }
}

impl<'content> FromOcamlRep for OcamlStr<'content> {
    fn from_ocamlrep(value: ocamlrep::Value<'_>) -> Result<Self, ocamlrep::FromError> {
        Ok(Self(unsafe {
            std::mem::transmute(ocamlrep::bytes_from_ocamlrep(value)?)
        }))
    }
}

#[repr(C)]
pub struct CErrBuf {
    pub buf: *mut c_char,
    pub buf_len: c_int,
}

#[repr(C)]
struct CNativeEnv {
    decl_getter:
        unsafe extern "C" fn(*const std::ffi::c_void, *const c_char) -> *const std::ffi::c_void,
    decl_provider: *const std::ffi::c_void,
    filepath: *const c_char,
    aliased_namespaces: *const c_char,
    include_roots: *const c_char,
    emit_class_pointers: i32,
    check_int_overflow: i32,
    hhbc_flags: u32,
    parser_flags: u32,
    flags: u8,
}
impl CNativeEnv {
    /// Returns `None` if `env` is null.
    ///
    /// # Safety
    /// * `env` must be a valid, aligned pointer to a `CEnv` which is not
    ///   accessed through another pointer for lifetime `'a` (note that this
    ///   lifetime is arbitrarily chosen by the caller)
    /// * Contents of the CEnv must be valid nul-terminated C strings
    ///   containing valid UTF-8, or arrays of same
    #[cfg(unix)]
    pub unsafe fn to_compile_env<'a>(
        env: &CNativeEnv,
    ) -> Option<hhbc_by_ref_compile::NativeEnv<&'a str>> {
        use std::os::unix::ffi::OsStrExt;

        Some(hhbc_by_ref_compile::NativeEnv {
            filepath: RelativePath::make(
                oxidized::relative_path::Prefix::Dummy,
                std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
                    std::ffi::CStr::from_ptr(env.filepath).to_bytes(),
                )),
            ),
            aliased_namespaces: std::str::from_utf8_unchecked(
                std::ffi::CStr::from_ptr(env.aliased_namespaces).to_bytes(),
            ),
            include_roots: std::str::from_utf8_unchecked(
                std::ffi::CStr::from_ptr(env.include_roots).to_bytes(),
            ),
            emit_class_pointers: env.emit_class_pointers,
            check_int_overflow: env.check_int_overflow,
            hhbc_flags: hhbc_by_ref_compile::HHBCFlags::from_bits(env.hhbc_flags)?,
            parser_flags: hhbc_by_ref_compile::ParserFlags::from_bits(env.parser_flags)?,
            flags: hhbc_by_ref_compile::EnvFlags::from_bits(env.flags)?,
        })
    }
}

fn decl_provider<'a, 'decl>(
    native_env: &hhbc_by_ref_compile::NativeEnv<&'a str>,
    cnative_env: &CNativeEnv,
) -> unified_decl_provider::DeclProvider<'decl> {
    if native_env
        .flags
        .contains(hhbc_by_ref_compile::EnvFlags::ENABLE_DECL)
    {
        unified_decl_provider::DeclProvider::ExternalDeclProvider(ExternalDeclProvider::new(
            cnative_env.decl_getter,
            cnative_env.decl_provider,
        ))
    } else {
        unified_decl_provider::DeclProvider::NoDeclProvider(NoDeclProvider)
    }
}

#[no_mangle]
unsafe extern "C" fn hackc_compile_hhas_create_arena() -> *mut bumpalo::Bump {
    Box::into_raw(Box::new(bumpalo::Bump::new()))
}

#[no_mangle]
unsafe extern "C" fn hackc_compile_hhas_free_arena(arena: *mut bumpalo::Bump) {
    let _ = Box::from_raw(arena);
}

// Compile to HHAS from source text.
#[no_mangle]
unsafe extern "C" fn hackc_compile_hhas_from_text_cpp_ffi(
    alloc: *const bumpalo::Bump,
    cnative_env: *const CNativeEnv,
    source_text: *const c_char,
    err_buf: *const CErrBuf,
) -> *const HhasProgram<'static> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Safety: `alloc` came via `hackc_compile_hhas_create_arena`.
        let alloc: &bumpalo::Bump = alloc.as_ref().unwrap();
        // Safety: `cnative_env`is a well aligned, properly initialized
        // `*const CNativeEnv`.
        let cnative_env: &CNativeEnv = cnative_env.as_ref().unwrap();
        // Safety: `err_buf` is a well aligned, properly initialized
        // `*const CErrBuf`.
        let err_buf: &CErrBuf = err_buf.as_ref().unwrap();
        // Safety : `err_buf.buf` must be valid for reads and writes
        // for `err_buf.buf_len * mem::sizeof::<u8>()` bytes.
        let buf: &mut [u8] =
            std::slice::from_raw_parts_mut(err_buf.buf as *mut u8, err_buf.buf_len as usize);
        // Safety: `source_text` is a properly iniitalized
        // nul-terminated C string.
        let text: &[u8] = std::ffi::CStr::from_ptr(source_text).to_bytes();

        match stack_limit::with_elastic_stack(
            |stack_limit| -> Result<*const HhasProgram<'static>, anyhow::Error> {
                let native_env = CNativeEnv::to_compile_env(cnative_env).unwrap();
                let env = hhbc_by_ref_compile::Env::<&str> {
                    filepath: native_env.filepath.clone(),
                    config_jsons: vec![],
                    config_list: vec![],
                    flags: native_env.flags,
                };
                let source_text = SourceText::make(RcOc::new(env.filepath.clone()), text);
                let compile_result = hhbc_by_ref_compile::hhas_from_text(
                    alloc,
                    &env,
                    &stack_limit,
                    source_text,
                    Some(&native_env),
                    decl_provider(&native_env, &cnative_env),
                );
                match compile_result {
                    Ok((hhas_prog, _)) => Ok(Box::into_raw(Box::new(hhas_prog))),
                    Err(e) => Err(anyhow!("{}", e)),
                }
            },
        )
        .map_err(|e| format!("{}", e))
        .expect("hackc_compile_hhas_from_text_cpp_ffi: retry failed")
        .map_err(|e| e.to_string())
        {
            Ok(hhas_prog) => hhas_prog,
            Err(e) => {
                if e.len() >= buf.len() {
                    warn!("Provided error buffer too small.");
                    warn!(
                        "Expected at least {} bytes but got {}.",
                        e.len() + 1,
                        buf.len()
                    );
                } else {
                    // Safety:
                    //   - `e` must be valid for reads of `e.len() *
                    //     size_of::<u8>()` bytes;
                    //   - `buf` must be valid for writes of of `e.len() *
                    //     size_of::<u8>()` bytes;
                    //   - The region of memory beginning at `e` with a
                    //     size of of `e.len() * size_of::<u8>()` bytes must
                    //     not overlap with the region of memory beginning
                    //     at `buf` with the same size;
                    //   - Even if the of `e.len() * size_of::<u8>()` is
                    //     `0`, the pointers must be non-null and properly
                    //     aligned.
                    std::ptr::copy_nonoverlapping(e.as_ptr(), buf.as_mut_ptr(), e.len());
                    buf[e.len()] = 0;
                }
                std::ptr::null()
            }
        }
    })) {
        Ok(hhas_prog) => hhas_prog,
        Err(_) => {
            if std::env::var_os("HH_TEST_MODE").is_some() {
                eprintln!("hackc_compile_hhas_from_text_cpp_ffi: panic!");
            }
            std::ptr::null()
        }
    }
}

#[no_mangle]
unsafe extern "C" fn hackc_compile_hhas_free_prog_cpp_ffi(prog: *mut HhasProgram) {
    let _ = Box::from_raw(prog);
}

#[no_mangle]
extern "C" fn compile_from_text_ffi(
    env: usize,
    rust_output_config: usize,
    source_text: usize,
) -> usize {
    ocamlrep_ocamlpool::catch_unwind_with_handler(
        || {
            let r: Result<(), String> = stack_limit::with_elastic_stack(|stack_limit| {
                let source_text = unsafe { SourceText::from_ocaml(source_text).unwrap() };
                let output_config =
                    unsafe { RustOutputConfig::from_ocaml(rust_output_config).unwrap() };
                let env = unsafe { hhbc_by_ref_compile::Env::<OcamlStr>::from_ocaml(env).unwrap() };
                let mut w = String::new();
                let alloc = bumpalo::Bump::new();
                match hhbc_by_ref_compile::from_text(
                    &alloc,
                    &env,
                    stack_limit,
                    &mut w,
                    source_text,
                    None,
                    unified_decl_provider::DeclProvider::NoDeclProvider(NoDeclProvider),
                ) {
                    Ok(profile) => print_output(
                        w,
                        output_config,
                        &env.filepath,
                        profile.map(|p| (p.parsing_t, p.codegen_t, p.parsing_t)),
                    ),
                    Err(e) => Err(anyhow!("{}", e)),
                }
            })
            .map_err(|e| format!("{}", e))
            .expect("Retry Failed")
            .map_err(|e| e.to_string());
            unsafe { to_ocaml(&r) }
        },
        // This handler is to catch `panic` from parser,
        // TODO(hrust): parser shouldn't panic instead it should return result
        // and then revert this diff.
        |panic_msg: &str| -> Result<usize, String> {
            let output_config =
                unsafe { RustOutputConfig::from_ocaml(rust_output_config).unwrap() };

            let mut w = String::new();
            let env = unsafe { hhbc_by_ref_compile::Env::<OcamlStr>::from_ocaml(env).unwrap() };
            hhbc_by_ref_compile::emit_fatal_program(&env, &mut w, panic_msg)
                .and_then(|_| print_output(w, output_config, &env.filepath, None))
                .map(|_| unsafe { to_ocaml(&<Result<(), String>>::Ok(())) })
                .map_err(|e| e.to_string())
        },
    )
}

fn print_output(
    bytecode: String,
    config: RustOutputConfig,
    file: &RelativePath,
    // TODO:(shiqicao) change following tuple to Profile after hhbc remove
    profile: Option<(f64, f64, f64)>,
) -> Result<()> {
    fn insert(o: &mut Map<String, Value>, k: impl Into<String>, v: impl Into<Value>) {
        o.insert(k.into(), v.into());
    }

    let mut writer: Box<dyn Write> = match config.output_file {
        Some(file) => Box::new(std::fs::File::create(file)?),
        None => Box::new(std::io::stdout()),
    };
    if config.include_header {
        let mut obj = Map::new();
        let to_microsec = |x| (x * 1_000_000.0) as u64;
        if let Some((parsing_t, codegen_t, printing_t)) = profile {
            insert(&mut obj, "parsing_time", to_microsec(parsing_t));
            insert(&mut obj, "codegen_time", to_microsec(codegen_t));
            insert(&mut obj, "printing_time", to_microsec(printing_t));
        }
        insert(
            &mut obj,
            "file",
            file.to_absolute()
                .to_str()
                .ok_or_else(|| anyhow!("invalid char in file path"))?,
        );
        insert(&mut obj, "type", "success");
        insert(&mut obj, "bytes", bytecode.as_bytes().len());
        write!(writer, "{}\n", Value::Object(obj))?;
    }

    writer.write_all(bytecode.as_bytes())?;
    writer.flush()?;
    Ok(())
}

#[no_mangle]
unsafe extern "C" fn hackc_hhas_to_string_cpp_ffi(
    cnative_env: *const CNativeEnv,
    prog: *const HhasProgram<'static>,
    err_buf: *const CErrBuf,
) -> *const c_char {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Safety: `prog`is a well aligned, properly initialized
        // `*const HhasProgram`.
        let prog = prog.as_ref().unwrap();
        // Safety: `cnative_env`is a well aligned, properly initialized
        // `*const CNativeEnv`.
        let cnative_env = cnative_env.as_ref().unwrap();

        // Safety : `err_buf.buf` must be valid for reads and writes
        // for `err_buf.buf_len * mem::sizeof::<u8>()` bytes.
        let buf_len: c_int = (*err_buf).buf_len;
        let buf: &mut [u8] =
            std::slice::from_raw_parts_mut((*err_buf).buf as *mut u8, buf_len as usize);

        let native_env: hhbc_by_ref_compile::NativeEnv<&str> =
            CNativeEnv::to_compile_env(cnative_env).unwrap();
        let env = hhbc_by_ref_compile::Env::<&str> {
            filepath: native_env.filepath.clone(),
            config_jsons: vec![],
            config_list: vec![],
            flags: native_env.flags,
        };
        let mut output = String::new();
        let compile_result =
            hhbc_by_ref_compile::hhas_to_string(&env, Some(&native_env), &mut output, prog);
        match compile_result {
            Ok(_) => {
                let cs = std::ffi::CString::new(output)
                    .expect("compile_ffi: hackc_hhas_to_string_cpp_ffi: String::new failed");
                cs.into_raw() as *const c_char
            }
            Err(e) => {
                let e = e.to_string();
                if e.len() >= buf.len() {
                    warn!("Provided error buffer too small.");
                    warn!(
                        "Expected at least {} bytes but got {}.",
                        e.len() + 1,
                        buf.len()
                    );
                } else {
                    /*
                    Safety:
                      - `e` must be valid for reads of `e.len() *
                        size_of::<u8>()` bytes;
                      - `buf` must be valid for writes of of `e.len() *
                        size_of::<u8>()` bytes;
                      - The region of memory beginning at `e` with a
                        size of of `e.len() * size_of::<u8>()` bytes must
                        not overlap with the region of memory beginning
                        at `buf` with the same size;
                      - Even if the of `e.len() * size_of::<u8>()` is
                        `0`, the pointers must be non-null and properly
                        aligned.
                    */
                    std::ptr::copy_nonoverlapping(e.as_ptr(), buf.as_mut_ptr(), e.len());
                    buf[e.len()] = 0;
                }
                std::ptr::null::<_>()
            }
        }
    })) {
        Ok(ptr) => ptr,
        _ => {
            if std::env::var_os("HH_TEST_MODE").is_some() {
                eprintln!("Error: panic in ffi function hackc_hhas_to_string_cpp_ffi");
            }
            std::ptr::null()
        }
    }
}

#[no_mangle]
unsafe extern "C" fn hackc_hhas_to_string_free_string_cpp_ffi(s: *mut c_char) {
    /*
    Safety:
      - This should only ever be called on a pointer obtained by
        `CString::into_raw`.
      - `CString::from_raw` and `CString::to_raw` should not be
        used with C functions that can modify the string's length.
    */
    let _ = std::ffi::CString::from_raw(s);
}

ocamlrep_ocamlpool::ocaml_ffi! {
  fn desugar_and_print_expr_trees(env: hhbc_by_ref_compile::Env<OcamlStr>) {
    hhbc_by_ref_compile::dump_expr_tree::desugar_and_print(&env);
  }
}
