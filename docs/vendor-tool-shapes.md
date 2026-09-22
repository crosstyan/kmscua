# Vendor computer-use tool shapes

Models are RL-tuned on the exact tool names, parameter spellings and return conventions
their vendor ships. A third-party server that copies those shapes gets the trained
behaviour for free; one that renames `mouse_button` to `button` does not. This file
records the shapes as of 2026-09, from official docs and clean-room captures of the
official `tools/list` output. Unverified details are marked.

## 1. Side by side

| Concept | Anthropic `computer_toolset_20260801` / Claude Code MCP | Codex macOS/Windows ("Sky", 10 tools) | OpenAI Responses `computer` (9 actions) | Gemini 3 computer use (browser) |
|---|---|---|---|---|
| Observe | `screenshot {}`; `zoom {region:[x0,y0,x1,y1]}` | `get_app_state {app, disableDiff?}` → AX tree text + window screenshot | `screenshot` | screenshot returned after every action |
| Apps | Claude Code: `request_access {apps[], reason, …}`, `list_granted_applications`, `open_application {app}`, `switch_display` | `list_apps {}` | none | `navigate`, `go_back`, `go_forward` |
| Click | `left_click / right_click / middle_click / double_click / triple_click {coordinate:[x,y], text?}` | `click {app, element_index?:string, x?, y?, click_count?, mouse_button?}` | `click {x,y,button,keys?}`, `double_click` | `click / double_click / right_click {x,y (0-999)}` |
| Move | `mouse_move {coordinate}` | none | `move {x,y,keys?}` | `move {x,y}` |
| Drag | `left_click_drag {start_coordinate?, coordinate, text?}`; `left_mouse_down/up {}` | `drag {app, from_x, from_y, to_x, to_y}` | `drag {path:[{x,y}…], keys?}` | `drag_and_drop` (legacy) |
| Scroll | `scroll {coordinate?, scroll_direction, scroll_amount:int, text?}` | `scroll {app, element_index, direction, pages?:number}` | `scroll {x,y,scroll_x,scroll_y,keys?}` | `scroll {x,y,direction,magnitude_in_pixels}` |
| Type | `type {text}` | `type_text {app, text}`; `set_value {app, element_index, value}`; `select_text {…}` | `type {text}` | `type {text, press_enter?}` |
| Keys | `key {text:"ctrl+s", repeat?}`; `hold_key {text, duration}` | `press_key {app, key}` (xdotool syntax) | `keypress {keys:[…]}` | `key_combination` (legacy) |
| AX actions | none | `perform_secondary_action {app, element_index, action}` | none | none |
| Wait | `wait {duration}` | none | `wait {}` (~2 s) | `wait {seconds?}` |
| Batch | Claude Code `computer_batch {actions:[…]}`; API: several `tool_use` blocks per turn | none | `computer_call.actions[]` | one per call |
| Clipboard | Claude Code `read_clipboard`, `write_clipboard {text}` | none | none | none |

## 2. Claude family (what kmscua ships)

`computer_toolset_20260801` is no longer one tool with an `action` enum: each member is
its own tool (`toolset_name: "computer"`). Members: `screenshot, zoom, left_click,
right_click, middle_click, double_click, triple_click, left_click_drag, mouse_move,
left_mouse_down, left_mouse_up, cursor_position, scroll, type, key, hold_key, wait`.

Parameter vocabulary:

```
coordinate        [x, y] in screenshot pixels
start_coordinate  [x, y], left_click_drag only; omit to drag from the current cursor
text              type: the text | key/hold_key: xdotool chord | click/scroll: modifiers to hold
scroll_direction  up | down | left | right
scroll_amount     integer ticks, 0-100
duration          seconds, 0-100 (hold_key, wait)
repeat            1-100 (key)
region            [x0, y0, x1, y1] (zoom)
```

Claude Code's own MCP keeps the legacy single-tool schema for `computer_batch` items:

```json
{"action": {"enum": ["key","type","mouse_move","left_click","left_click_drag","right_click",
  "middle_click","double_click","triple_click","scroll","hold_key","screenshot",
  "cursor_position","left_mouse_down","left_mouse_up","wait"]},
 "coordinate": [x, y], "start_coordinate": [x, y], "text": "...",
 "scroll_direction": "...", "scroll_amount": 0, "duration": 0, "repeat": 1}
```

Return conventions:

- `screenshot` → one image block, nothing else.
- Every other action → the text `OK`.
- `cursor_position` → `X=512, Y=384`.
- A failed batch item stops the batch; later items answer
  `Not executed: an earlier computer action in this turn failed.`
- Coordinates are full-display screenshot pixels. After `zoom`, coordinates still refer
  to the full-screen screenshot, never the zoomed image.
- Screenshot scale: `min(1, 2576/long_edge, sqrt(3.75MP/total_px))`; 1080p is the
  documented cost/accuracy balance, 720p and 1366x768 are the cheap options.
- Descriptions that matter: "The returned image is what subsequent click coordinates are
  relative to"; xdotool key names (`Return`, `ctrl+s`, `alt+Tab`, `Page_Down`).

Claude Code adds `request_access`, `list_granted_applications`, `open_application`,
`switch_display`, `read_clipboard`, `write_clipboard`, and hides non-approved apps from
the compositor. Not implemented in kmscua.

Sources: https://platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool ,
https://code.claude.com/docs/en/computer-use , Claude Code `computer-use-mcp` tool source
as mirrored at https://github.com/claude-code-best/claude-code (unofficial mirror).

## 3. Codex family (TODO in kmscua)

App-scoped and accessibility-first. Ten tools, each description ends with
"This tool is part of plugin `Computer Use`.", all schemas `additionalProperties: false`.

```jsonc
list_apps   {}                     // running + used in the last 14 days, with usage counts
get_app_state {app, disableDiff?}  // "must be called once per assistant turn before interacting"
click       {app, element_index?: string, x?, y?, click_count?: int, mouse_button?: left|right|middle}
drag        {app, from_x, from_y, to_x, to_y}
scroll      {app, element_index: string, direction, pages?: number (fractional ok)}
type_text   {app, text}
set_value   {app, element_index, value}
select_text {app, element_index, text, prefix?, suffix?, selection_type?: text|cursor_before|cursor_after}  // schema unverified
press_key   {app, key}             // xdotool syntax: "a", "Return", "Tab", "super+c", "Up", "KP_0"
perform_secondary_action {app, element_index, action}   // e.g. Expand, Raise, Show Menu
```

`get_app_state` returns `[text, image]`. The text grammar:

```
App=com.apple.finder (pid 1106)
Window: "open-codex-computer-use", App: Finder.
    0 standard window open-codex-computer-use, ID: FinderWindow, Secondary Actions: Raise
        1 split group
            2 scroll area
                3 outline sidebar
                    4 row (selectable, expanded) Value: Favorites, Secondary Actions: Collapse
           47 search text field (settable, string)
The focused UI element is 2 outline.
```

Line grammar: `<index> <role words> (<states>) <title>, Value: …, ID: …, Description: …,
Help: …, Placeholder: …, Secondary Actions: A, B`, four-space indent per depth, indices in
DFS order, stable only within one snapshot (diffs keep IDs of unchanged nodes). Every
action returns a refreshed, diffed tree plus screenshot, not `OK`. Errors are text with
`isError: true`: `appNotFound("iTerm2")`, `Computer Use is not active for '{app}'. You
first must call get_app_state…`. Coordinates are window-scoped screenshot pixels.

The bundled SKILL.md tells the model: call `get_app_state` first and again after every
action; prefer `element_index` over coordinates; the tree comes back as a diff unless
`disableDiff: true`; `app` may be a display name, path, process name or bundle id; do not
guess `perform_secondary_action` names.

Mapping onto Linux: AT-SPI2 roles, states and `Action` interface map onto the grammar
directly; `set_value` and `select_text` map to `EditableText` / `Text`; the window
screenshot is a kmscua region crop at the AT-SPI window extents. This is the TODO.

Sources: https://learn.chatgpt.com/docs/computer-use , clean-room captures in
https://github.com/iFurySt/open-codex-computer-use (`ToolDefinitions.swift`),
https://github.com/egoist/waku (Sky reverse engineering, SKILL.md copy),
https://github.com/TheGuyWithoutH/mac-computer-use (SPECS.md).

## 4. OpenAI Responses API `computer`

`click{x,y,button: left|right|wheel|back|forward, keys?}`, `double_click{x,y,keys?}`,
`drag{path:[{x,y}], keys?}`, `keypress{keys:[]}`, `move{x,y,keys?}`, `screenshot{}`,
`scroll{x,y,scroll_x,scroll_y,keys?}`, `type{text}`, `wait{}`. The caller answers with
`computer_call_output {output: {type: "computer_screenshot", image_url: data-url}}` and
must acknowledge `pending_safety_checks`.

Source: https://developers.openai.com/api/docs/guides/tools-computer-use-integration

## 5. Gemini 3 computer use

Browser only. Coordinates on a 0-999 normalized grid. Actions: `open_web_browser`,
`click_at`, `type_text_at`, `scroll_document`, `drag_and_drop`, `key_combination`,
`wait_5_seconds`, `navigate`, `go_back`, `go_forward`. `safety_decision:
require_confirmation` must be acknowledged.

Source: https://ai.google.dev/gemini-api/docs/computer-use

## 6. Rules kmscua follows

1. Copy names and parameter spellings exactly, including the odd ones.
2. Flat tools, not a `computer {action}` enum, except inside `computer_batch`.
3. Actions answer `OK`; `screenshot` is image only; `cursor_position` is `X=…, Y=…`.
4. Extras (`settle`, `wait_for_stable`, `wait_for_change`, `record_*`) are separate tools
   or optional parameters, so the trained vocabulary stays intact.
5. Descriptions are prompt text; reuse the vendor wording where it carries a rule.
