//! Xcode index store reader: libIndexStore.dylib loaded with dlopen from the active toolchain.
//! The C API is declared by hand (Xcode ships no header); see spike/indexstore/isread.c.
//! Read-only: a DataStore is only ever opened for reading, and nothing here triggers a build.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};

type Ptr = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct StrRef {
    data: *const c_char,
    length: usize,
}
impl StrRef {
    fn bytes(&self) -> &[u8] {
        if self.data.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(self.data as *const u8, self.length) } }
    }
    fn string(&self) -> String {
        String::from_utf8_lossy(self.bytes()).into_owned()
    }
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> Ptr;
    fn dlsym(h: Ptr, name: *const c_char) -> Ptr;
}

// SymbolRole bits (indexstore).
pub const R_DECL: u64 = 1 << 0;
pub const R_DEF: u64 = 1 << 1;
pub const R_REF: u64 = 1 << 2;
pub const R_CALL: u64 = 1 << 5;
pub const R_IMPLICIT: u64 = 1 << 8;
pub const REL_CHILD_OF: u64 = 1 << 9;
pub const REL_BASE_OF: u64 = 1 << 10;
pub const REL_OVERRIDE_OF: u64 = 1 << 11;
pub const REL_CALLED_BY: u64 = 1 << 13;
pub const REL_EXTENDED_BY: u64 = 1 << 14;
pub const REL_ACCESSOR_OF: u64 = 1 << 15;
pub const REL_CONTAINED_BY: u64 = 1 << 16;

macro_rules! lib {
    ($($f:ident : $t:ty = $n:expr),* $(,)?) => {
        struct Lib { $($f: $t),* }
        impl Lib {
            unsafe fn bind(h: Ptr) -> Result<Lib> {
                Ok(Lib { $($f: {
                    let s = CString::new($n).unwrap();
                    let p = unsafe { dlsym(h, s.as_ptr()) };
                    if p.is_null() { bail!("libIndexStore is missing {}", $n); }
                    unsafe { std::mem::transmute::<Ptr, $t>(p) }
                }),* })
            }
        }
    };
}

type Cb1 = extern "C" fn(Ptr, Ptr) -> bool;
lib! {
    store_create: unsafe extern "C" fn(*const c_char, *mut Ptr) -> Ptr = "indexstore_store_create",
    error_desc: unsafe extern "C" fn(Ptr) -> *const c_char = "indexstore_error_get_description",
    units_apply: unsafe extern "C" fn(Ptr, c_int, Ptr, extern "C" fn(Ptr, StrRef) -> bool) -> bool = "indexstore_store_units_apply_f",
    unit_create: unsafe extern "C" fn(Ptr, *const c_char, *mut Ptr) -> Ptr = "indexstore_unit_reader_create",
    unit_dispose: unsafe extern "C" fn(Ptr) = "indexstore_unit_reader_dispose",
    unit_deps: unsafe extern "C" fn(Ptr, Ptr, Cb1) -> bool = "indexstore_unit_reader_dependencies_apply_f",
    unit_module: unsafe extern "C" fn(Ptr) -> StrRef = "indexstore_unit_reader_get_module_name",
    dep_kind: unsafe extern "C" fn(Ptr) -> c_int = "indexstore_unit_dependency_get_kind",
    dep_name: unsafe extern "C" fn(Ptr) -> StrRef = "indexstore_unit_dependency_get_name",
    dep_path: unsafe extern "C" fn(Ptr) -> StrRef = "indexstore_unit_dependency_get_filepath",
    dep_system: unsafe extern "C" fn(Ptr) -> bool = "indexstore_unit_dependency_is_system",
    rec_create: unsafe extern "C" fn(Ptr, *const c_char, *mut Ptr) -> Ptr = "indexstore_record_reader_create",
    rec_dispose: unsafe extern "C" fn(Ptr) = "indexstore_record_reader_dispose",
    rec_occs: unsafe extern "C" fn(Ptr, Ptr, Cb1) -> bool = "indexstore_record_reader_occurrences_apply_f",
    occ_sym: unsafe extern "C" fn(Ptr) -> Ptr = "indexstore_occurrence_get_symbol",
    occ_roles: unsafe extern "C" fn(Ptr) -> u64 = "indexstore_occurrence_get_roles",
    occ_line_col: unsafe extern "C" fn(Ptr, *mut u32, *mut u32) = "indexstore_occurrence_get_line_col",
    occ_rels: unsafe extern "C" fn(Ptr, Ptr, Cb1) -> bool = "indexstore_occurrence_relations_apply_f",
    rel_roles: unsafe extern "C" fn(Ptr) -> u64 = "indexstore_symbol_relation_get_roles",
    rel_sym: unsafe extern "C" fn(Ptr) -> Ptr = "indexstore_symbol_relation_get_symbol",
    sym_usr: unsafe extern "C" fn(Ptr) -> StrRef = "indexstore_symbol_get_usr",
    sym_name: unsafe extern "C" fn(Ptr) -> StrRef = "indexstore_symbol_get_name",
    sym_kind: unsafe extern "C" fn(Ptr) -> c_int = "indexstore_symbol_get_kind",
    sym_subkind: unsafe extern "C" fn(Ptr) -> c_int = "indexstore_symbol_get_subkind",
    sym_lang: unsafe extern "C" fn(Ptr) -> c_int = "indexstore_symbol_get_language",
}

/// Trampoline: run a Rust closure from a C callback.
extern "C" fn tramp<F: FnMut(Ptr) -> bool>(ctx: Ptr, x: Ptr) -> bool {
    let f = unsafe { &mut *(ctx as *mut F) };
    f(x)
}
extern "C" fn tramp_str<F: FnMut(StrRef) -> bool>(ctx: Ptr, x: StrRef) -> bool {
    let f = unsafe { &mut *(ctx as *mut F) };
    f(x)
}

fn cb1<F: FnMut(Ptr) -> bool>(f: &mut F) -> (Ptr, Cb1) {
    (f as *mut F as Ptr, tramp::<F>)
}
fn cbs<F: FnMut(StrRef) -> bool>(f: &mut F) -> (Ptr, extern "C" fn(Ptr, StrRef) -> bool) {
    (f as *mut F as Ptr, tramp_str::<F>)
}

pub fn kind_name(k: i32) -> &'static str {
    match k {
        1 => "module",
        2 => "namespace",
        3 => "namespacealias",
        4 => "macro",
        5 => "enum",
        6 => "struct",
        7 => "class",
        8 => "protocol",
        9 => "extension",
        10 => "union",
        11 => "typealias",
        12 => "func",
        13 => "var",
        14 => "field",
        15 => "case",
        16 => "method",
        17 => "classmethod",
        18 => "staticmethod",
        19 => "property",
        20 => "classproperty",
        21 => "staticproperty",
        22 => "init",
        23 => "deinit",
        24 => "conversion",
        25 => "param",
        _ => "other",
    }
}

#[derive(Debug, Clone)]
pub struct SymInfo {
    pub usr: String,
    /// Full indexstore name, e.g. `play(item:)`.
    pub name: String,
    pub kind: i32,
    pub subkind: i32,
    pub lang: i32,
}

/// One occurrence row; `rel` is the related symbol (callee's caller, base, extension, ...) if any.
#[derive(Debug, Clone, Copy)]
pub struct OccRow {
    pub sym: u32,
    pub roles: u64,
    pub line: u32,
    pub col: u32,
    pub rel_roles: u64,
    pub rel_sym: u32,
}
pub const NONE: u32 = u32::MAX;

/// A source file's data from one unit.
pub struct FileRec {
    pub path: String,
    pub occs: Vec<OccRow>,
}

pub struct UnitInfo {
    pub name: String,
    pub module: String,
    pub files: Vec<(String, String)>, // (source path, record name)
}

pub struct Store {
    lib: Lib,
    store: Ptr,
    pub dir: PathBuf,
    /// USR -> id into `syms`; shared across everything read through this handle.
    pub syms: Vec<SymInfo>,
    by_usr: HashMap<Vec<u8>, u32>,
}

fn toolchain_dylib() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![];
    if let Some(d) = std::env::var_os("FOXDB_INDEXSTORE_LIB") {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("DEVELOPER_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if cfg!(target_os = "macos")
        && let Ok(o) = std::process::Command::new("xcode-select").arg("-p").output()
        && o.status.success()
    {
        dirs.push(PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    }
    dirs.push(PathBuf::from("/Applications/Xcode.app/Contents/Developer"));
    dirs.into_iter().map(|d| d.join("Toolchains/XcodeDefault.xctoolchain/usr/lib/libIndexStore.dylib")).find(|p| p.exists())
}

impl Store {
    /// `dir` is the DataStore directory (the one containing `v5`).
    pub fn open(dir: &Path) -> Result<Store> {
        let Some(dy) = toolchain_dylib() else { bail!("libIndexStore.dylib not found (is Xcode installed?)") };
        let cdy = CString::new(dy.to_string_lossy().as_bytes())?;
        let h = unsafe { dlopen(cdy.as_ptr(), 1) };
        if h.is_null() {
            bail!("dlopen {} failed", dy.display());
        }
        let lib = unsafe { Lib::bind(h)? };
        let cdir = CString::new(dir.to_string_lossy().as_bytes())?;
        let mut err: Ptr = std::ptr::null_mut();
        let store = unsafe { (lib.store_create)(cdir.as_ptr(), &mut err) };
        if store.is_null() {
            let msg = if err.is_null() {
                "unknown".into()
            } else {
                unsafe { CStr::from_ptr((lib.error_desc)(err)) }.to_string_lossy().into_owned()
            };
            bail!("cannot open index store {}: {msg}", dir.display());
        }
        Ok(Store { lib, store, dir: dir.to_path_buf(), syms: vec![], by_usr: HashMap::new() })
    }

    pub fn unit_names(&self) -> Vec<String> {
        let mut v: Vec<String> = vec![];
        let mut f = |s: StrRef| {
            v.push(s.string());
            true
        };
        let (c, t) = cbs(&mut f);
        unsafe { (self.lib.units_apply)(self.store, 0, c, t) };
        v
    }

    /// Source files (Swift only) and their record names for a unit.
    pub fn unit_info(&self, name: &str) -> Option<UnitInfo> {
        let cname = CString::new(name).ok()?;
        let mut err: Ptr = std::ptr::null_mut();
        let u = unsafe { (self.lib.unit_create)(self.store, cname.as_ptr(), &mut err) };
        if u.is_null() {
            return None;
        }
        let module = unsafe { (self.lib.unit_module)(u) }.string();
        let lib = &self.lib;
        let mut files = vec![];
        let mut f = |d: Ptr| {
            if unsafe { (lib.dep_kind)(d) } == 2 && !unsafe { (lib.dep_system)(d) } {
                let p = unsafe { (lib.dep_path)(d) }.string();
                if p.ends_with(".swift") {
                    files.push((p, unsafe { (lib.dep_name)(d) }.string()));
                }
            }
            true
        };
        let (c, t) = cb1(&mut f);
        unsafe { (self.lib.unit_deps)(u, c, t) };
        unsafe { (self.lib.unit_dispose)(u) };
        Some(UnitInfo { name: name.to_string(), module, files })
    }

    fn intern(&mut self, sym: Ptr) -> u32 {
        let usr = unsafe { (self.lib.sym_usr)(sym) };
        if let Some(&i) = self.by_usr.get(usr.bytes()) {
            return i;
        }
        let info = SymInfo {
            usr: usr.string(),
            name: unsafe { (self.lib.sym_name)(sym) }.string(),
            kind: unsafe { (self.lib.sym_kind)(sym) },
            subkind: unsafe { (self.lib.sym_subkind)(sym) },
            lang: unsafe { (self.lib.sym_lang)(sym) },
        };
        let id = self.syms.len() as u32;
        self.by_usr.insert(usr.bytes().to_vec(), id);
        self.syms.push(info);
        id
    }

    /// Read every occurrence (with its relations) in a record.
    pub fn read_record(&mut self, record: &str) -> Option<Vec<OccRow>> {
        let cname = CString::new(record).ok()?;
        let mut err: Ptr = std::ptr::null_mut();
        let r = unsafe { (self.lib.rec_create)(self.store, cname.as_ptr(), &mut err) };
        if r.is_null() {
            return None;
        }
        let mut out: Vec<OccRow> = vec![];
        // Raw copy of the fn table so the closure does not borrow `self` while interning.
        let this: *mut Store = self;
        let mut f = |o: Ptr| {
            let s = unsafe { &mut *this };
            let sym = unsafe { (s.lib.occ_sym)(o) };
            let sid = s.intern(sym);
            let roles = unsafe { (s.lib.occ_roles)(o) };
            let (mut line, mut col) = (0u32, 0u32);
            unsafe { (s.lib.occ_line_col)(o, &mut line, &mut col) };
            let mut nrel = 0;
            let mut g = |rel: Ptr| {
                let s = unsafe { &mut *this };
                let rr = unsafe { (s.lib.rel_roles)(rel) };
                let rs = unsafe { (s.lib.rel_sym)(rel) };
                let rid = s.intern(rs);
                out.push(OccRow { sym: sid, roles, line, col, rel_roles: rr, rel_sym: rid });
                nrel += 1;
                true
            };
            let (c, t) = cb1(&mut g);
            unsafe { (s.lib.occ_rels)(o, c, t) };
            if nrel == 0 {
                out.push(OccRow { sym: sid, roles, line, col, rel_roles: 0, rel_sym: NONE });
            }
            true
        };
        let (c, t) = cb1(&mut f);
        unsafe { (self.lib.rec_occs)(r, c, t) };
        unsafe { (self.lib.rec_dispose)(r) };
        Some(out)
    }
}
