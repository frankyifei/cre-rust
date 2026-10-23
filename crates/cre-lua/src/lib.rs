use std::ffi::{c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::ptr;

use cre_lib::{Document, DocumentFormat, DocumentOptions};
use mlua::ffi as lua;
use mlua::lua_State;

const CREDOC: &[u8] = b"credocument\0";
const INDEX: &[u8] = b"__index\0";

#[repr(C)]
struct BlitBuffer {
    w: u32,
    pixel_stride: u32,
    h: u32,
    stride: usize,
    data: *mut u8,
    config: u8,
}

struct Doc {
    inner: Option<Document>,
    width: u32,
    height: u32,
    layout_height: u32,
    margins: (i32, i32, i32, i32),
    font_face: String,
    font_size: i32,
    path: Option<PathBuf>,
}

impl Doc {
    fn new(width: u32, height: u32) -> Self {
        Self {
            inner: None,
            width: width.max(1),
            height: height.max(1),
            layout_height: 100_000,
            margins: (0, 0, 0, 0),
            font_face: "Noto Serif".to_string(),
            font_size: 22,
            path: None,
        }
    }

    fn open(&mut self, path: PathBuf) -> anyhow::Result<()> {
        let options = DocumentOptions {
            width: self.width,
            height: self.height,
            layout_height: self.layout_height,
            page_margin: self.margins.0.max(0) as f32,
            font_path: None,
        };
        self.inner = Some(Document::with_options(&path, &options)?);
        self.path = Some(path);
        Ok(())
    }

    fn relayout(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let xpointer = self.inner.as_ref().and_then(|inner| {
            inner
                .xpointer_from_page(inner.current_page())
                .ok()
                .filter(|xp| !xp.is_empty())
        });
        let old = self.inner.take();
        if self.open(path).is_err() {
            self.inner = old;
            return;
        }
        if let (Some(xpointer), Some(inner)) = (xpointer, self.inner.as_mut()) {
            let _ = inner.go_to_xpointer(&xpointer);
        }
    }

    fn current_page(&self) -> usize {
        self.inner.as_ref().map_or(1, Document::current_page)
    }

    fn page_count(&self) -> usize {
        self.inner.as_ref().map_or(0, Document::page_count)
    }
}

unsafe fn doc_ref<'a>(l: *mut lua_State) -> Option<&'a Doc> {
    // SAFETY: methods registered in the `credocument` metatable always receive our userdata.
    let ptr = unsafe { lua::lua_touserdata(l, 1) }.cast::<Doc>();
    // SAFETY: the userdata was initialized by `new_doc_view`.
    unsafe { ptr.as_ref() }
}

unsafe fn doc_mut<'a>(l: *mut lua_State) -> Option<&'a mut Doc> {
    // SAFETY: methods registered in the `credocument` metatable always receive our userdata.
    let ptr = unsafe { lua::lua_touserdata(l, 1) }.cast::<Doc>();
    // SAFETY: the userdata was initialized by `new_doc_view`.
    unsafe { ptr.as_mut() }
}

unsafe fn push_str(l: *mut lua_State, value: &str) {
    // SAFETY: `value` is valid for the duration of the call and Lua copies it.
    unsafe { lua::lua_pushlstring_(l, value.as_ptr().cast(), value.len()) };
}

unsafe fn arg_str(l: *mut lua_State, index: c_int) -> Option<String> {
    let mut len = 0;
    // SAFETY: `index` is a valid Lua stack index for this call.
    let ptr = unsafe { lua::lua_tolstring(l, index, &mut len) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Lua guarantees `ptr` points to `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

unsafe fn arg_int(l: *mut lua_State, index: c_int) -> i64 {
    // SAFETY: `index` is a valid Lua stack index for this call.
    unsafe { lua::lua_tointeger_(l, index) }
}

unsafe fn arg_bool(l: *mut lua_State, index: c_int) -> bool {
    // SAFETY: `index` is a valid Lua stack index for this call.
    unsafe { lua::lua_toboolean(l, index) != 0 }
}

unsafe fn push_bool(l: *mut lua_State, value: bool) {
    // SAFETY: Lua takes the boolean by value.
    unsafe { lua::lua_pushboolean(l, c_int::from(value)) };
}

unsafe fn push_int(l: *mut lua_State, value: i64) {
    // SAFETY: Lua takes the integer by value.
    unsafe { lua::lua_pushinteger(l, value) };
}

unsafe fn format_name(format: DocumentFormat) -> &'static str {
    match format {
        DocumentFormat::Epub => "EPUB",
        DocumentFormat::Fb2 => "FB2",
        DocumentFormat::Txt => "TXT",
    }
}

unsafe extern "C-unwind" fn stub(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn return_one(_l: *mut lua_State) -> c_int {
    1
}

unsafe extern "C-unwind" fn return_true(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_pushboolean(l, 1) };
    1
}

unsafe extern "C-unwind" fn return_false(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_pushboolean(l, 0) };
    1
}

unsafe extern "C-unwind" fn return_zero(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_pushinteger(l, 0) };
    1
}

unsafe extern "C-unwind" fn return_empty_string(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_str(l, "") };
    1
}

unsafe extern "C-unwind" fn return_empty_table(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_newtable(l) };
    1
}

unsafe extern "C-unwind" fn return_arg2(l: *mut lua_State) -> c_int {
    // SAFETY: the argument may be absent; `lua_pushvalue` preserves nil.
    unsafe { lua::lua_pushvalue(l, 2) };
    1
}

unsafe extern "C-unwind" fn module_noop(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn new_doc_view(l: *mut lua_State) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let width = arg_int(l, 1).max(1) as u32;
            let height = arg_int(l, 2).max(1) as u32;
            let ptr = lua::lua_newuserdata(l, std::mem::size_of::<Doc>()).cast::<Doc>();
            ptr::write(ptr, Doc::new(width, height));
            lua::luaL_getmetatable(l, CREDOC.as_ptr().cast());
            lua::lua_setmetatable(l, -2);
        }
        1
    }));
    result.unwrap_or_else(|_| {
        // SAFETY: push nil to signal failure.
        unsafe { lua::lua_pushnil(l) };
        1
    })
}

unsafe extern "C-unwind" fn doc_gc(l: *mut lua_State) -> c_int {
    // SAFETY: `__gc` receives a userdata initialized by `new_doc_view`.
    unsafe {
        let ptr = lua::lua_touserdata(l, 1).cast::<Doc>();
        if !ptr.is_null() {
            ptr::drop_in_place(ptr);
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_load(l: *mut lua_State) -> c_int {
    let loaded = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return false;
            };
            let Some(path) = arg_str(l, 2) else {
                return false;
            };
            doc.open(PathBuf::from(path)).is_ok()
        }
    }))
    .unwrap_or(false);
    // SAFETY: `l` is the active Lua state.
    unsafe { push_bool(l, loaded) };
    1
}

unsafe extern "C-unwind" fn doc_close(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.inner = None;
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_render(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            if let Some(doc) = doc_mut(l)
                && let Some(inner) = doc.inner.as_mut()
            {
                let _ = inner.render_current_page();
            }
        }
    }));
    0
}

unsafe extern "C-unwind" fn doc_format(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let name = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .map(|inner| format_name(inner.metadata().format))
            .unwrap_or("TXT");
        push_str(l, name);
    }
    1
}

unsafe extern "C-unwind" fn doc_props(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        lua::lua_createtable(l, 0, 7);
        let mut props = [
            ("title", ""),
            ("authors", ""),
            ("language", ""),
            ("series", ""),
            ("description", ""),
            ("keywords", ""),
            ("identifiers", ""),
        ];
        if let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) {
            let meta = inner.metadata();
            props[0].1 = meta.title.as_deref().unwrap_or("");
            props[1].1 = meta.creator.as_deref().unwrap_or("");
            props[2].1 = meta.language.as_deref().unwrap_or("");
        }
        for (key, value) in props {
            push_str(l, key);
            push_str(l, value);
            lua::lua_rawset(l, -3);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_pages(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let pages = doc_ref(l).map_or(0, |doc| doc.page_count());
        push_int(l, pages as i64);
    }
    1
}

unsafe extern "C-unwind" fn doc_current_page(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = doc_ref(l).map_or(1, |doc| doc.current_page());
        push_int(l, page as i64);
    }
    1
}

unsafe extern "C-unwind" fn doc_goto_page(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let Some(inner) = doc.inner.as_mut() else {
                return;
            };
            let page = arg_int(l, 2).max(1) as usize;
            let _ = inner.go_to_page(page);
        }
    }));
    0
}

unsafe extern "C-unwind" fn doc_goto_percent(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let count = doc.page_count();
            let Some(inner) = doc.inner.as_mut() else {
                return;
            };
            if count == 0 {
                return;
            }
            let percent = arg_int(l, 2).clamp(0, 10_000) as usize;
            let page = (percent * count / 10_000).max(1).min(count);
            let _ = inner.go_to_page(page);
        }
    }));
    0
}

unsafe extern "C-unwind" fn doc_goto_pos(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let count = doc.page_count();
            let height = doc.height.max(1) as i64;
            let Some(inner) = doc.inner.as_mut() else {
                return;
            };
            if count == 0 {
                return;
            }
            let pos = arg_int(l, 2).max(0);
            let page = ((pos / height) as usize + 1).min(count);
            let _ = inner.go_to_page(page);
        }
    }));
    0
}

unsafe extern "C-unwind" fn doc_goto_xpointer(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let Some(xpointer) = arg_str(l, 2) else {
                return;
            };
            if let Some(inner) = doc.inner.as_mut() {
                let _ = inner.go_to_xpointer(&xpointer);
            }
        }
    }));
    0
}

unsafe extern "C-unwind" fn doc_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let xpointer = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.xpointer_from_page(inner.current_page()).ok())
            .unwrap_or_default();
        push_str(l, &xpointer);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2).max(1) as usize;
        let xpointer = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.xpointer_from_page(page).ok())
            .unwrap_or_default();
        push_str(l, &xpointer);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_str(l, 2)
            .and_then(|xp| {
                doc_ref(l)
                    .and_then(|doc| doc.inner.as_ref())
                    .and_then(|inner| inner.page_from_xpointer(&xp).ok())
            })
            .unwrap_or(1);
        push_int(l, page as i64);
    }
    1
}

unsafe extern "C-unwind" fn doc_xpointer_in_document(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let found = arg_str(l, 2).is_some_and(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.page_from_xpointer(&xp).ok())
                .is_some()
        });
        push_bool(l, found);
    }
    1
}

unsafe extern "C-unwind" fn doc_xpointer_in_current_page(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let found = arg_str(l, 2).is_some_and(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .is_some_and(|inner| {
                    inner
                        .page_from_xpointer(&xp)
                        .ok()
                        .is_some_and(|page| page == inner.current_page())
                })
        });
        push_bool(l, found);
    }
    1
}

unsafe extern "C-unwind" fn doc_pos_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let (y, x) = arg_str(l, 2)
            .and_then(|xp| {
                doc_ref(l).and_then(|doc| {
                    let page = doc.inner.as_ref()?.page_from_xpointer(&xp).ok()?;
                    Some(((page.saturating_sub(1) * doc.height as usize) as i64, 0))
                })
            })
            .unwrap_or((0, 0));
        push_int(l, y);
        push_int(l, x);
    }
    2
}

unsafe extern "C-unwind" fn doc_current_pos(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let pos = doc_ref(l).map_or(0, |doc| {
            doc.current_page().saturating_sub(1) as i64 * i64::from(doc.height)
        });
        push_int(l, pos);
    }
    1
}

unsafe extern "C-unwind" fn doc_current_percent(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let percent = doc_ref(l).map_or(0, |doc| {
            let count = doc.page_count();
            if count == 0 {
                0
            } else {
                doc.current_page() as i64 * 10_000 / count as i64
            }
        });
        push_int(l, percent);
    }
    1
}

unsafe extern "C-unwind" fn doc_full_height(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let height = doc_ref(l).map_or(0, |doc| doc.page_count() as i64 * i64::from(doc.height));
        push_int(l, height);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_start_y(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2).max(1) as usize;
        let y = doc_ref(l).map_or(0, |doc| {
            page.saturating_sub(1) as i64 * i64::from(doc.height)
        });
        push_int(l, y);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_height(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let height = doc_ref(l).map_or(0, |doc| i64::from(doc.height));
        push_int(l, height);
    }
    1
}

unsafe extern "C-unwind" fn doc_font_size(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let size = doc_ref(l).map_or(22, |doc| doc.font_size);
        push_int(l, i64::from(size));
    }
    1
}

unsafe extern "C-unwind" fn doc_font_face(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let face = doc_ref(l).map_or("", |doc| doc.font_face.as_str());
        push_str(l, face);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_margins(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let (left, top, right, bottom) = doc_ref(l).map_or((0, 0, 0, 0), |doc| doc.margins);
        lua::lua_createtable(l, 0, 4);
        for (key, value) in [
            ("left", left),
            ("top", top),
            ("right", right),
            ("bottom", bottom),
        ] {
            push_str(l, key);
            push_int(l, i64::from(value));
            lua::lua_rawset(l, -3);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_toc(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            lua::lua_newtable(l);
            return 1;
        };
        lua::lua_createtable(l, inner.toc().len() as c_int, 0);
        for (index, item) in inner.toc().iter().enumerate() {
            lua::lua_createtable(l, 0, 4);
            push_str(l, "page");
            push_int(l, item.page.unwrap_or(1) as i64);
            lua::lua_rawset(l, -3);
            push_str(l, "xpointer");
            push_str(l, item.xpointer.as_deref().unwrap_or(&item.href));
            lua::lua_rawset(l, -3);
            push_str(l, "depth");
            push_int(l, item.depth as i64);
            lua::lua_rawset(l, -3);
            push_str(l, "title");
            push_str(l, &item.title);
            lua::lua_rawset(l, -3);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_draw_current_page(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state. The BlitBuffer pointer is supplied by KOReader
        // through `lua_topointer`, matching cre.cpp.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let Some(inner) = doc.inner.as_mut() else {
                return;
            };
            let bb = lua::lua_topointer(l, 2).cast::<BlitBuffer>();
            if bb.is_null() || (*bb).data.is_null() {
                return;
            }
            let color = arg_bool(l, 3);
            let Ok(bitmap) = inner.render_current_page() else {
                return;
            };
            let bb = &*bb;
            let rows = bitmap.height.min(bb.h) as usize;
            let cols = bitmap.width.min(bb.w) as usize;
            let dst =
                std::slice::from_raw_parts_mut(bb.data, bb.stride.saturating_mul(bb.h as usize));
            if color {
                for y in 0..rows {
                    let src = &bitmap.data[y * bitmap.width as usize..];
                    let dst_row = &mut dst[y * bb.stride..];
                    for (x, &value) in src.iter().take(cols).enumerate() {
                        let offset = x * 4;
                        dst_row[offset] = value;
                        dst_row[offset + 1] = value;
                        dst_row[offset + 2] = value;
                        dst_row[offset + 3] = 0xFF;
                    }
                }
            } else {
                for y in 0..rows {
                    let src = &bitmap.data[y * bitmap.width as usize..];
                    let dst_row = &mut dst[y * bb.stride..];
                    dst_row[..cols].copy_from_slice(&src[..cols]);
                }
            }
        }
    }));
    // SAFETY: `l` is the active Lua state.
    unsafe {
        push_int(l, 0);
        lua::lua_pushnumber(l, 0.0);
    }
    2
}

unsafe extern "C-unwind" fn doc_find_text(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(query) = arg_str(l, 2) else {
            return 0;
        };
        let requested = arg_int(l, 7);
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            return 0;
        };
        let Ok(hits) = inner.find_text(&query) else {
            return 0;
        };
        if hits.is_empty() {
            return 0;
        }
        let limit = if requested <= 0 {
            hits.len()
        } else {
            hits.len().min(requested as usize)
        };
        lua::lua_createtable(l, limit as c_int, 0);
        for (index, hit) in hits.iter().take(limit).enumerate() {
            push_match(l, hit, None);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
        push_int(l, limit as i64);
    }
    2
}

unsafe fn push_match(l: *mut lua_State, hit: &cre_lib::SearchHit, matched_text: Option<&str>) {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        lua::lua_createtable(l, 0, 7);
        push_str(l, "start");
        push_str(l, &hit.xpointer_range.start.to_xpointer());
        lua::lua_rawset(l, -3);
        push_str(l, "end");
        let end = hit.xpointer_range.end.as_ref().map_or_else(
            || hit.xpointer_range.start.to_xpointer(),
            |end| end.to_xpointer(),
        );
        push_str(l, &end);
        lua::lua_rawset(l, -3);
        if let Some(text) = matched_text {
            push_str(l, "matched_text");
            push_str(l, text);
            lua::lua_rawset(l, -3);
            push_str(l, "prev_text");
            push_str(l, "");
            lua::lua_rawset(l, -3);
            push_str(l, "matched_word_prefix");
            push_str(l, "");
            lua::lua_rawset(l, -3);
            push_str(l, "matched_word_suffix");
            push_str(l, "");
            lua::lua_rawset(l, -3);
        }
    }
}

unsafe extern "C-unwind" fn doc_find_all_text(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(query) = arg_str(l, 2) else {
            return 0;
        };
        let requested = arg_int(l, 5);
        let get_matched_text = arg_bool(l, 6);
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            return 0;
        };
        let Ok(hits) = inner.find_text(&query) else {
            return 0;
        };
        if hits.is_empty() {
            return 0;
        }
        let limit = if requested <= 0 {
            hits.len()
        } else {
            hits.len().min(requested as usize)
        };
        lua::lua_createtable(l, limit as c_int, 0);
        for (index, hit) in hits.iter().take(limit).enumerate() {
            push_match(l, hit, get_matched_text.then_some(query.as_str()));
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
        push_int(l, limit as i64);
    }
    2
}

unsafe extern "C-unwind" fn doc_word_from_position(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let hit = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner
                .hit_test(cre_lib::PagePosition {
                    page: inner.current_page(),
                    x,
                    y,
                })
                .ok()
                .flatten()
        });
        let Some(hit) = hit else {
            lua::lua_newtable(l);
            return 1;
        };
        lua::lua_createtable(l, 0, 5);
        push_str(l, "word");
        push_str(l, &hit.text);
        lua::lua_rawset(l, -3);
        for (key, value) in [
            ("x0", hit.rect.x),
            ("y0", hit.rect.y),
            ("x1", hit.rect.x + hit.rect.width),
            ("y1", hit.rect.y + hit.rect.height),
        ] {
            push_str(l, key);
            push_int(l, value.round() as i64);
            lua::lua_rawset(l, -3);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_text_from_positions(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let hit = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner
                .hit_test(cre_lib::PagePosition {
                    page: inner.current_page(),
                    x,
                    y,
                })
                .ok()
                .flatten()
        });
        let Some(hit) = hit else {
            return 0;
        };
        lua::lua_createtable(l, 0, 4);
        push_str(l, "text");
        push_str(l, &hit.text);
        lua::lua_rawset(l, -3);
        push_str(l, "pos0");
        push_str(l, "");
        lua::lua_rawset(l, -3);
        push_str(l, "pos1");
        push_str(l, "");
        lua::lua_rawset(l, -3);
        push_str(l, "title");
        push_str(l, "");
        lua::lua_rawset(l, -3);
    }
    1
}

unsafe extern "C-unwind" fn doc_link_from_position(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let link = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner
                .hit_test(cre_lib::PagePosition {
                    page: inner.current_page(),
                    x,
                    y,
                })
                .ok()
                .flatten()
                .and_then(|hit| hit.link)
        });
        let Some(link) = link else {
            return 0;
        };
        push_str(l, &link);
    }
    1
}

unsafe extern "C-unwind" fn doc_set_view_dimen(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let width = arg_int(l, 2).max(1) as u32;
            let height = arg_int(l, 3).max(1) as u32;
            if (width, height) != (doc.width, doc.height) {
                doc.width = width;
                doc.height = height;
                if doc.inner.is_some() {
                    doc.relayout();
                }
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_page_margins(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let margins = (
                arg_int(l, 2) as i32,
                arg_int(l, 3) as i32,
                arg_int(l, 4) as i32,
                arg_int(l, 5) as i32,
            );
            if margins != doc.margins {
                doc.margins = margins;
                if doc.inner.is_some() {
                    doc.relayout();
                }
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_font_face(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.font_face = arg_str(l, 2).unwrap_or_default();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_font_size(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.font_size = arg_int(l, 2).clamp(1, 512) as i32;
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_get_int_property(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_int(l, 0) };
    1
}

unsafe extern "C-unwind" fn doc_get_string_property(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_str(l, "") };
    1
}

unsafe extern "C-unwind" fn doc_get_page_links(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_newtable(l) };
    1
}

unsafe extern "C-unwind" fn doc_get_document_file_content(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_str(l, "") };
    1
}

unsafe extern "C-unwind" fn doc_get_text_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_str(l, "") };
    1
}

unsafe extern "C-unwind" fn doc_compare_xpointers(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_int(l, 0) };
    1
}

unsafe extern "C-unwind" fn doc_visible_page_count(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_int(l, 1) };
    1
}

unsafe extern "C-unwind" fn doc_render_hash(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_int(l, 1) };
    1
}

unsafe extern "C-unwind" fn doc_set_background_color(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn doc_check_regex(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_bool(l, true) };
    1
}

unsafe extern "C-unwind" fn doc_read_defaults(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_newtable(l) };
    1
}

macro_rules! reg {
    ($name:literal, $func:path) => {
        lua::luaL_Reg {
            name: concat!($name, "\0").as_ptr().cast::<c_char>(),
            func: $func,
        }
    };
}

macro_rules! regs {
    ($(($name:literal, $func:path)),* $(,)?) => {
        [
            $(reg!($name, $func),)*
            lua::luaL_Reg {
                name: ptr::null(),
                func: stub,
            },
        ]
    };
}

const DOC_METHODS: &[lua::luaL_Reg] = &regs![
    ("loadDocument", doc_load),
    ("renderDocument", doc_render),
    ("requestRender", stub),
    ("getIntProperty", doc_get_int_property),
    ("getStringProperty", doc_get_string_property),
    ("getDocumentFormat", doc_format),
    ("getDocumentProps", doc_props),
    ("setAltDocumentProp", stub),
    ("getDocumentRenderingHash", doc_render_hash),
    ("canBePartiallyRerendered", return_false),
    ("isPartialRerenderingEnabled", return_false),
    ("enablePartialRerendering", return_false),
    ("getPartialRerenderingsCount", return_zero),
    ("isRerenderingDelayed", return_false),
    ("getPages", doc_pages),
    ("getCurrentPage", doc_current_page),
    ("getPageFlow", return_zero),
    ("getPageFromXPointer", doc_page_from_xpointer),
    ("getPosFromXPointer", doc_pos_from_xpointer),
    ("getCurrentPos", doc_current_pos),
    ("getCurrentPercent", doc_current_percent),
    ("getXPointer", doc_xpointer),
    ("getPageXPointer", doc_page_xpointer),
    ("getEPubCFI", return_empty_string),
    ("getPageEPubCFI", return_empty_string),
    ("getEPubCFIFromXPointer", return_empty_string),
    ("getEPubCFIRangeFromXPointers", return_empty_string),
    ("getPageOffsetX", return_zero),
    ("getPageStartY", doc_page_start_y),
    ("getPageHeight", doc_page_height),
    ("getFullHeight", doc_full_height),
    ("getFontSize", doc_font_size),
    ("getFontFace", doc_font_face),
    ("getEmbeddedFontList", return_empty_table),
    ("getPageMargins", doc_page_margins),
    ("getHeaderHeight", return_zero),
    ("getToc", doc_toc),
    ("getVisiblePageCount", doc_visible_page_count),
    ("getVisiblePageNumberCount", doc_visible_page_count),
    ("getNextVisibleWordStart", return_arg2),
    ("getNextVisibleWordEnd", return_arg2),
    ("getPrevVisibleWordStart", return_arg2),
    ("getPrevVisibleWordEnd", return_arg2),
    ("getPrevVisibleChar", return_arg2),
    ("getNextVisibleChar", return_arg2),
    ("getTextFromXPointers", return_empty_string),
    ("compareXPointers", doc_compare_xpointers),
    ("setIntProperty", stub),
    ("setStringProperty", stub),
    ("setViewMode", stub),
    ("setViewDimen", doc_set_view_dimen),
    ("setHeaderInfo", stub),
    ("setPageInfoOverride", stub),
    ("setHeaderFont", stub),
    ("setHeaderProgressMarks", stub),
    ("setFontFace", doc_set_font_face),
    ("setFontSize", doc_set_font_size),
    ("setDefaultInterlineSpace", stub),
    ("setStyleSheet", stub),
    ("setEmbeddedStyleSheet", stub),
    ("setEmbeddedFonts", stub),
    ("setBackgroundColor", doc_set_background_color),
    ("setBackgroundImage", stub),
    ("setPageMargins", doc_set_page_margins),
    ("setVisiblePageCount", stub),
    ("adjustFontSizes", stub),
    ("setBatteryState", stub),
    ("setCallback", stub),
    ("isBuiltDomStale", return_false),
    ("hasCacheFile", return_false),
    ("isCacheFileStale", return_false),
    ("invalidateCacheFile", stub),
    ("getCacheFilePath", return_empty_string),
    ("updateTocAndPageMap", stub),
    ("getStatistics", return_empty_string),
    ("getUnknownEntities", return_empty_string),
    ("buildAlternativeToc", stub),
    ("isTocAlternativeToc", return_false),
    ("gotoPage", doc_goto_page),
    ("gotoPercent", doc_goto_percent),
    ("gotoPos", doc_goto_pos),
    ("gotoXPointer", doc_goto_xpointer),
    ("zoomFont", doc_font_size),
    ("drawCurrentPage", doc_draw_current_page),
    ("findText", doc_find_text),
    ("findAllText", doc_find_all_text),
    ("isXPointerInCurrentPage", doc_xpointer_in_current_page),
    ("isXPointerInDocument", doc_xpointer_in_document),
    ("getLinkFromPosition", doc_link_from_position),
    ("getWordFromPosition", doc_word_from_position),
    ("getTextFromPositions", doc_text_from_positions),
    ("extendXPointersToSentenceSegment", return_arg2),
    ("getWordBoxesFromPositions", return_empty_table),
    ("getImageDataFromPosition", stub),
    ("getNearestWordFromPosition", return_empty_table),
    ("getLangTagForTextFromPosition", return_empty_string),
    ("getDocumentFileContent", doc_get_document_file_content),
    ("getTextFromXPointer", doc_get_text_from_xpointer),
    ("getHTMLFromXPointer", return_empty_string),
    ("getHTMLFromXPointers", return_empty_string),
    ("getStylesheetsMatchingRulesets", return_empty_table),
    ("getPageLinks", doc_get_page_links),
    ("isLinkToFootnote", return_false),
    ("highlightXPointer", stub),
    ("getNormalizedXPointer", return_arg2),
    ("getCoverPageImageData", stub),
    ("gotoLink", stub),
    ("goBack", stub),
    ("goForward", stub),
    ("clearSelection", stub),
    ("hasPageMap", return_false),
    ("getPageMap", return_empty_table),
    ("getPageMapSource", return_empty_string),
    ("getPageMapFirstPageLabel", return_empty_string),
    ("getPageMapLastPageLabel", return_empty_string),
    ("getPageMapCurrentPageLabel", return_empty_string),
    ("getPageMapXPointerPageLabel", return_empty_string),
    ("getPageMapVisiblePageLabels", return_empty_table),
    ("buildSyntheticPageMapIfNoneDocumentProvided", stub),
    ("isPageMapSynthetic", return_true),
    ("buildSyntheticPageMap", stub),
    ("getSyntheticPageMapCharsPerPage", return_zero),
    ("hasPageMapDocumentProvided", return_false),
    ("isPageMapDocumentProvided", return_false),
    ("hasNonLinearFlows", return_false),
    ("hasHiddenFlows", return_false),
    ("checkRegex", doc_check_regex),
    ("getAndClearRegexSearchError", return_empty_string),
    ("readDefaults", doc_read_defaults),
    ("saveDefaults", stub),
    ("close", doc_close),
    ("__gc", doc_gc),
];

const MODULE_FUNCS: &[lua::luaL_Reg] = &regs![
    ("initCache", module_noop),
    ("initHyphDict", module_noop),
    ("newDocView", new_doc_view),
    ("getFontFaces", return_empty_table),
    ("getFontFaceFilenameAndFaceIndex", return_empty_table),
    ("getFontFaceAvailableWeights", return_empty_table),
    ("getGammaLevel", return_zero),
    ("getGammaIndex", return_zero),
    ("setGammaIndex", module_noop),
    ("registerFont", module_noop),
    ("regularizeRegisteredFontsWeights", module_noop),
    ("setAsPreferredFontWithBias", module_noop),
    ("getHyphDictList", return_empty_table),
    ("getSelectedHyphDict", return_empty_string),
    ("setHyphDictionary", module_noop),
    ("getTextLangStatus", return_zero),
    ("getLatestDomVersion", return_zero),
    ("getDomVersionWithNormalizedXPointers", return_zero),
    ("setUserHyphenationDict", module_noop),
    ("getHyphenationForWord", return_empty_string),
    ("softHyphenateText", return_arg2),
    ("renderImageData", module_noop),
    ("getBalancedHTML", return_empty_string),
    ("smoothScaleBlitBuffer", return_one),
    ("setImageReplacementChar", module_noop),
    ("setSkipTearDown", module_noop),
    ("setViewMode", module_noop),
    ("setTocAlternativeToc", module_noop),
    ("getDocumentRenderingHash", return_zero),
];

/// Opens the `libkoreader-cre` Lua module.
///
/// # Safety
///
/// `l` must be a valid `lua_State` supplied by the LuaJIT module loader.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn luaopen_cre(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is a valid Lua state supplied by the LuaJIT module loader.
    unsafe {
        lua::luaL_newmetatable_(l, CREDOC.as_ptr().cast());
        lua::lua_pushvalue(l, -1);
        lua::lua_setfield(l, -2, INDEX.as_ptr().cast());
        lua::luaL_register(l, ptr::null(), DOC_METHODS.as_ptr());
        lua::lua_pop(l, 1);

        lua::lua_newtable(l);
        lua::luaL_register(l, ptr::null(), MODULE_FUNCS.as_ptr());
    }
    1
}
