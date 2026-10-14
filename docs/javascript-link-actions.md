# JavaScript link actions

PdfCraft inspects `/Subtype /Link /A /S /JavaScript` without rewriting the action dictionary.
`/A` and `/JS` may be indirect objects. `/JS` may be a PDF string or a decoded stream;
the inspection reader caps decoded bytes at 256 KiB and uses the common PDF text decoder.
Malformed, cyclic and oversized operands remain unsupported actions, never executable source.

## Execution path

The renderer exposes `LinkTarget::JavaScript(source)`. The canvas keeps the link's page and
raw annotation index and asks the engine to prepare immutable `LinkScript` input. The engine
captures the source document ID and generation, document information, form fields and named
document scripts. Reading a document does not require an editor; field changes still require
the existing editing permissions.

The native script worker owns its Boa context throughout one action. Document scripts initialize
that context under `Doc/Open`; the action then runs under `Link/Mouse Up`, with `event.target`
equal to the document, `event.targetName` empty, and `pageNum` initially the link's page.
The link event does not use `event.rc` as a cancellation result.

`app.popUpMenuEx` sends a plain menu tree to the frontend and waits only on the worker thread.
The frontend polls between frames and returns a selection or cancellation over a reply channel.
The same Boa invocation resumes: no replay of the action, no source transformation and no
special-case handling of document generators. `Function.prototype.apply` works normally.

## Desktop context menus

The canvas captures the activation position in egui viewport points before starting the worker.
The desktop host uses egui 0.36.2 `PopupKind::Menu`, anchored below/right with automatic
flipping and viewport constraints. Root menus and submenus have independently measured widths,
capped at 340 points; their scroll areas are capped at 420 points and the available height.
The menu has no dialog title or modal overlay. Labels wrap, disabled entries cannot activate,
marked entries show a check, and separators and nested submenus retain their PDF values.
Arrow keys navigate enabled entries, Right/Enter enter submenus, Left leaves, Enter/Space select,
and Escape cancels. Menu-owned focus is released when the interaction ends.

An outside press cancels the old interaction without consuming the pointer event: the canvas,
another annotation or an interface control receives that same press/release. Another link starts
a fresh invocation immediately when a worker slot is available. It never replays the old script.
Every retired menu receives null; all later menus from that execution are cancelled and its
completion is discarded, including field changes and external requests. Each invocation has its
own receiver and immutable engine identity/generation check. Frame identity prevents egui
multi-pass layout from starting the same activation twice.

At most three live execution channels are retained. If all slots are occupied, only the latest
activation is queued; it starts when a bounded worker finishes. The graphics thread only polls
channels. Closing/switching documents, changing page/zoom/rotation/layout, editing, disabling
JavaScript, opening another dialog, loss of window focus, outside scrolling, Tab and command
shortcuts cancel the interaction. Menu scrolling stays inside the menu. Escape is consumed;
outside pointer/scroll events and document shortcuts are left available to the reader.

The opt-in control channel reports `javascript_menu` in `ui.state`, with source document/page/
annotation, captured anchor, visible rectangles, execution sequence and live channel count.
It also reports window focus. This adds inspection only; no transport or authentication changes.

## API and limits

Menu entries support `cName`, `cReturn` (including the empty string), `bEnabled`, `bMarked`,
separators (`cName: "-"`), and `oSubMenu` as an object or array. Missing `cReturn` falls back
to `cName`. Cancellation returns JavaScript `null`. Replies are validated against enabled
leaves of the exact menu tree before resuming the interpreter.

Menus are limited to 1,024 entries, 16 submenu levels, 4,096 bytes per label/return value,
and 32 popup calls per action. Each reply wait expires after five minutes. Dropping the host
or its reply channel releases a waiting worker. Existing Boa limits remain: one million
loop iterations, recursion 256, bounded parser nesting, and a dedicated script stack.
Document initialization additionally permits at most 1,024 scripts totalling 1 MiB.

`pageNum` requests navigate the source document's view. `app.launchURL` uses the existing
document URL allowlist and explicit consent flow. `app.openDoc(cPath)` and
`app.openDoc({cPath: ...})` return a file-opening request; the UI asks consent even for
relative paths. No file is read by Boa. Windows `/C/path` and `/C:/path` forms normalize to
`C:/path` before the existing local PDF resolver. URL and network paths remain rejected.
Filesystem validation, PDF extension, regular-file, size and password checks still apply.
Other filesystems or an `oDoc` base option are refused.

The engine rechecks JavaScript enablement and the source generation before applying results.
The frontend cancels a pending menu on document close, edits, undo/redo or JavaScript disablement.
Form changes and resets use the existing single undoable batch. A failed link script does not
commit form changes or dispatch its requests; its errors and diagnostic output remain visible.

## Headless access

`link_run_javascript {doc, page, index, menu_choices?}` uses one-based `link_list` numbering.
It returns `menus`, `alerts`, `console`, `requests`, and `error`. `menu_choices` is an optional
array of `cReturn` strings or null, consumed in popup order, maximum 32 entries. Omitted choices
cancel. Invalid selections fail closed. External URL/file requests are returned to the caller
and never executed; consequently this tool does not read paths outside `--root`.

Engine APIs are `Session::prepare_link_javascript`, `LinkScript::start` (native),
`LinkScript::run` (headless cancellation), `Session::link_javascript_is_current`, and
`Session::finish_link_javascript`. Source identity and execution input are immutable.

## Compatibility boundaries

This extends the existing Acrobat subset rather than implementing the entire Acrobat host.
Named document scripts are initialized for each fresh action context, as existing form execution
does; globals are not persistent between actions. Catalog OpenAction and document/page lifecycle
actions, `/Next` action chains, other annotation triggers, named JavaScript destinations and
all other Acrobat APIs remain outside this implementation. `app.openDoc` returns `null`
immediately, not a live Doc object: opening is deferred to consent, and subsequent script access
to the opened document is unsupported. Hidden/password/options behavior is not emulated.
Native link menus are supported on desktop; the web UI explicitly reports them unavailable.
Existing synchronous console and button execution cancels popup requests instead of displaying
the native link menu. These boundaries must not be represented as complete Acrobat compatibility.

## Verification

Synthetic tests cover direct/indirect strings, compressed streams, UTF-16BE, malformed and
oversized operands, reference cycles, menu cancellation, nested/disabled items, forged replies,
continuation without replay, events, read-only documents, stale generations, disablement, and
save/reopen preservation. The optional ignored engine test
`external_pdf_javascript_actions_read_only` takes `PDFCRAFT_JS_READONLY_PDF`, exercises up to
ten links and checks the original file remains byte-identical. It never saves that file or
includes external content in the repository. UI tests live in `js_menu::tests`; they assert
single-click replacement, actual canvas activation at different zoom/DPI settings, placement,
cancellation, stale completions, bounded rapid activation, keyboard/submenus and pending consent
without opening a target file. `PDFCRAFT_SHOT` and `PDFCRAFT_MENU_SHOTS` optionally save private
synthetic test renders. The pinned `egui_kittest` dependency is now available. See
[menu ergonomics audit](javascript-menu-ergonomics-audit.md) for the actual Windows validation
results and its remaining transport limitation; headless renders are not native validation.

Public API behavior source: [Acrobat JavaScript API Reference](https://opensource.adobe.com/dc-acrobat-sdk-docs/library/jsapiref/JS_API_AcroJS.html#popupmenuex).
No implementation code or assets from Adobe products were used.
