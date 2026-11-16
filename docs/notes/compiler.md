# Compiler and .fl notes

Language pitfalls and compiler changes (the language itself: docs/ferrite-lang.md).

## `.fl` pitfalls
- A generated on_click dispatcher calls **every** handler registered for a widget (ui.fl's back button uses `ui_back_custom` for apps that override it); arrays can't be passed to functions; a global may not share a name with any function's param/local (`color`, `radius` in ui.fl).
- Images: `MAX_IMAGES` 64 with `pool_large`/`host` (8 otherwise); `Image::draw` takes signed coords and clips to the scissor/screen (a picture sliding in from off-screen).

## Compiler: untyped float locals
- An unannotated `var` (type `any`) initialized from a float expression is now typed float (`CodeGen._stored_type`), locals and globals. Before, `var c = itof(n); var d = sin(a); c + d` compiled to the **int** add (any + any → int path, and `BIN_V` with superops). Checked: every other example compiles to identical bytecode.

## Compiler: classes (2026-11-16)
- `class Name { var field...; fn method() {...} }` (docs/ferrite-lang.md "Classes"). Compiler-only, no opcodes or caps: an instance is a `T_ARRAY` pool block, one word per field. The parser lowers a class (`_lower_class`) into ordinary `FnDef`s: methods become `Name.method(self: Name, ...)`, the constructor is a function named `Name` (init's params) that runs `__new("Name")` (alloc + field initializers + array fields, `CodeGen._gen_new`), calls `Name.init` and returns `self`. A class name is a type string in `var_types`/`param_types`; `CodeGen.classes` holds the layouts (`_register_classes`, after functions and globals are typed). `obj.field` on a class-typed variable → array ops with a constant index (`_gen_field_*`), else the widget property path. `obj.m(...)` parses to `CallExpr('.m', [obj, ...])`, resolved by the receiver's type (`_method_name`). `free(obj)` → `drop()`, array fields, instance.
- Limits: one-level access (`p.a.b`, `p.arr[i]` don't parse), no `p.x++`, no inheritance, manual lifetime. Adding classes left every example's bytecode identical.
- Test: ferrite-ui-xl `ferrite-core/testdata/classes/classes.fl` (`gen.py`) → `runtime::class_tests`.
