# UI Protocol Reference

The UI protocol lets a chat client show the screen that a configuration's [UI scripts](/v0/reference/ui) build, and report back what the user does. `agentctl chat` is one such client. Use this page when you write another.

The protocol runs over the `/ws` WebSocket, with the same request envelopes, tokens and cancellation as every other method. See the [WebSocket protocol reference](/v0/reference/protocol) for those. This page covers what is specific to the UI.

## Opening a chat

Send `ui.subscribe` when the user opens a chat. The request stays open, and its events carry the chat's screen until the chat closes.

```json
{ "id": 1, "method": "ui.subscribe", "params": {
  "version": 1,
  "client": { "kind": "web", "width": 120, "height": 40, "supports": [] },
  "session": "4f1c2a",
  "folder": "/home/me/project"
} }
```

| Param | Required | Description |
|---|---|---|
| `client` | yes | `kind`, a name for your client; `width` and `height`, the window size in cells; `supports`, the extra element names your client draws, if any |
| `version` | no | The UI protocol version your client speaks. The current version is `1` |
| `session` | no | The session the chat shows. Leave it out for a new chat |
| `folder` | no | For a new chat, the folder your client runs in. It becomes the chat's folder when the user trusts it, on the `ws` interface or an [admin connection](/v0/reference/protocol#admin-token-on-ws) |
| `user` | no | The end user, when your interface carries its own users |

A cell is one character's width, or one line's height. Pick a unit that maps to that in your client, such as `ch` and `em` on the web.

The daemon answers with a `ui.ready` event:

```json
{ "event": "ui.ready", "id": 1, "ready": { "version": 1, "chat": 7, "clock": 81234 } }
```

| Field | Description |
|---|---|
| `version` | The protocol version the daemon speaks |
| `chat` | The chat's id. Every other UI request names it |
| `clock` | The daemon's clock, in milliseconds |

Note the time on your own clock when `ui.ready` arrives. Together with `clock`, it tells you the daemon's time at any later moment, which you need to show frames on time.

To close the chat, cancel the request with `runners.cancel`, or close the connection. The request then ends with `{ "closed": true }`.

## Receiving frames

After `ui.ready`, the daemon sends `ui.frame` events. Each frame is a list of changes to the chat's screen:

```json
{ "event": "ui.frame", "id": 1, "frame": {
  "seq": 1, "at": 81234, "reset": true,
  "ops": [ { "op": "create", "node": 100, "kind": "text", "props": { "text": "hi" } } ]
} }
```

| Field | Description |
|---|---|
| `seq` | Increases with every frame |
| `at` | When to show the frame, on the daemon's clock |
| `reset` | When `true`, throw away the whole screen before applying the ops. They rebuild it |
| `cancel_ahead` | When `true`, drop the frames you received earlier whose `at` is later than this frame's |
| `ops` | The changes, to apply in order |

A frame can arrive before its time, so keep a queue ordered by `at`, then `seq`. Show each frame when the daemon's clock reaches its `at`. When several frames are due at once, apply them all and draw once.

The first frame always has `reset`. A later one has it too when the configuration reloads. On a reset, clear your queue, your focus and your scroll positions, and drop every entry decoration you were showing. Then send your conversation entries again, as in [Conversation entries](#conversation-entries).

## Applying ops

The screen is a tree of elements, each with a numeric `node` id. Every op has an `op` field naming what it does:

| Op | Fields | What to do |
|---|---|---|
| `create` | `node`, `kind`, `props`, `style` | Add a new element, outside the tree for now. `props` and `style` are left out when empty |
| `set` | `node`, `key`, `value` | Set a property. `null` removes it |
| `style` | `node`, `key`, `value` | Set a style. `null` removes it |
| `insert` | `parent`, `node`, `index` | Put the element under `parent` at `index`, counting from 0, moving it if it is elsewhere. An index past the end means the end |
| `remove` | `node` | Take the element out of its parent. It may be inserted again |
| `destroy` | `node` | Forget the element and everything under it |
| `root` | `node` | Show the tree under this element as the screen |
| `bindings` | `keys` | The keyboard shortcuts scripts bound, such as `["ctrl+g", "f2"]`. Replaces the previous list |
| `entry` | `key`, `decoration` | How to show one conversation entry, see [Conversation entries](#conversation-entries) |
| `screen` | `node`, `screen` | What a terminal element shows, see [Terminal elements](#terminal-elements) |
| `note` | `text`, `error` | A message for the user. `error` is `true` when it reports a failure, and is left out otherwise |

Ignore an op that names an element you do not have.

## Drawing elements

The elements, their properties, the styles, colours and sizes are the ones listed in the [UI scripting reference](/v0/reference/ui#reference). Draw them as it describes, and draw the screen under the current `root` only.

Lay out a `box` or `list` as a column, or as a row when its `direction` is `"row"`:

- A size that is a number is that many cells. A percentage is that share of the parent's space, after gaps. `"fill"` shares what is left equally among the `fill` children. With no size, an element takes what its content needs, except a `spacer` or `terminal`, which fill.
- Across the other direction, an element stretches to its parent's size unless it has a number or percentage.
- A border (any `border` other than `"none"`) takes one cell on every side, and `padding` takes that many more cells inside it. `gap` is the number of cells between children.
- An element whose `visible` is `false` takes no space.
- A container with `scroll` set to `true` lays its children out at the height they need and shows a scrollable window onto them.

Every screen starts with chat's own parts, at fixed node ids. Draw the ones whose kind starts with `chat.` your own way, in the space the layout gives them:

| Kind | Node | Id | What to draw |
|---|---|---|---|
| `box` | 1 | `chat` | The default screen's outermost column |
| `chat.header` | 2 | `header` | The header. Its children go at the right end of its first line, each as wide as it needs |
| `box` | 7 | `header.right` | A row inside the header, for script elements |
| `box` | 3 | `main` | The row that holds the conversation |
| `chat.conversation` | 4 | `conversation` | The conversation |
| `chat.prompt` | 5 | `prompt` | The message composer |
| `chat.status` | 6 | `status` | The status line |

Draw every string as text. Replace control characters before you show them, so that no script output reaches your terminal or page as markup or escape codes.

Draw only the elements you know. Skip a kind or a property you do not recognise.

## Reporting what the user does

Send `ui.event` for each thing the user does to an element. It names the chat, the element's `node`, the event `type`, and the event's own fields:

```json
{ "id": 12, "method": "ui.event", "params": { "chat": 7, "node": 104, "type": "click" } }
```

| Type | Fields | When |
|---|---|---|
| `click` | | The user clicks an element, or presses Enter or Space on a focused button or checkbox. Enter on a focused list sends `click` for its selected item |
| `change` | `value` | The user edits an input (`value` is its text), ticks a checkbox (`true` or `false`) or selects a list item (its position, from 1) |
| `key` | `key` | A key that is not typing, such as `enter` or `up`, in a focused input |
| `focus`, `blur` | | An element gains or loses focus |
| `hover` | | The mouse moves onto an element |
| `input` | `data` | Typed text, or the bytes a key sends, for a focused terminal element |
| `resize` | `width`, `height` | The window changed size, or with a terminal's `node`, the space you give that terminal |

Each request answers `{}`.

Send the deepest element under the pointer as `node`. The daemon passes the event up through the element's parents itself.

When you send `change`, apply the new value to your own copy first: set `value` on the input, `checked` on the checkbox, or `selected` on the list. The daemon takes the value without sending it back.

Focus is yours to keep. Give it to an `input`, `terminal`, `button`, `checkbox` or `list` when the user clicks it or something inside it, and send `blur` and `focus` when it moves. Clicking an item inside a list also selects that item. Clicking a checkbox flips its `checked`.

A `resize` without a `node` reports the whole window.

## Keyboard shortcuts

The latest `bindings` op lists the keys scripts bound. When the user presses one of them and no input or terminal has focus, send a `key` event without a `node`:

```json
{ "id": 13, "method": "ui.event", "params": { "chat": 7, "type": "key", "key": "ctrl+g" } }
```

Write key names in lower case, with modifiers first in the order `ctrl`, `alt`, `shift`, such as `ctrl+shift+k`. An upper-case letter is `shift` with the lower-case letter. Other keys are `f1` to `f12`, `enter`, `escape`, `tab`, `backspace`, `delete`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup` and `pagedown`.

Some keys stay with your client whatever the bindings say: Ctrl+C, Shift+Tab for switching the approval mode, Enter for sending the prompt, and every key of the approval window while it is open. Show the approval window whenever the agent asks, whatever the screen looks like.

## Conversation entries

Scripts can read and decorate the entries in your conversation, so tell the daemon about them.

Send `ui.session` when the chat opens another session, or starts a new one:

```json
{ "id": 20, "method": "ui.session", "params": { "chat": 7, "session": "9b2e01" } }
```

Leave out `session` for a new chat. The answer is `{ "folder": ... }`, the chat's folder after the switch, or `null` when it has none. A switch also clears every entry decoration.

Send `ui.entry` for each entry when it first appears, and again whenever it changes:

```json
{ "id": 21, "method": "ui.entry", "params": { "chat": 7, "entry": {
  "key": "e3", "kind": "tool", "name": "files.read", "input": { "path": "README.md" }
} } }
```

| Field | Description |
|---|---|
| `key` | A name you choose for the entry, unique in the chat until the next `ui.session` |
| `kind` | `user`, `agent`, `tool`, `info`, `error` or `setting` |
| `text` | The message, on every kind except `tool` |
| `name` | On a tool call: the action or tool called |
| `input` | On a tool call: its arguments |
| `result` | On a tool call: what it returned |
| `failed` | `true` when the tool call failed |
| `changes` | The files the tool call changed, in the shape of `changes` on [runner deltas](/v0/reference/protocol#runners-run) |

The daemon answers with `entry` ops. Each one carries the whole decoration for the entry with that `key`:

| Field | What to do |
|---|---|
| `summary` | On a collapsed tool row, show this line in place of the output |
| `hidden` | When `true`, hide the entry |
| `style` | Apply this style to the entry's text |
| `before`, `after` | Show these elements, by node id, above and below the entry |

A decoration with none of these fields means the entry goes back to how you show it by default.

## Terminal elements

A `terminal` element shows a program that runs on the daemon. Its screen arrives in `screen` ops:

| Field | Description |
|---|---|
| `width`, `height` | The screen's size in cells |
| `lines` | One list per row, of pieces of text that share a style |
| `cursor` | `[column, row]` of the cursor, from 0, when it is shown |

Each piece has `text`, and may have `fg`, `bg`, `bold`, `italic`, `underline` and `inverse`. A colour is `"#rrggbb"`, or `"@n"` for colour `n` of the 256-colour terminal palette. A missing colour is your default.

While a terminal has focus, send what the user types as `input` events with the bytes the program expects, escape sequences included. Send a `resize` with the terminal's `node` whenever the space you give it changes, and the program follows the new size. Until the first `screen` op arrives, show that the program is starting.

Let Esc reach the program. Give the user another key to leave a focused terminal, such as Ctrl+Q.

## Errors

| Code | Meaning |
|---|---|
| `ui_version` | The `version` you asked for is newer than the daemon's |
| `ui_chat_not_found` | The `chat` is not open on this connection. Each connection can only use the chats it opened |
| `bad_params` | The params did not match, such as an unknown field in `ui.subscribe`, `ui.session` or `ui.entry` |

Other error codes and the error envelope are in the [WebSocket protocol reference](/v0/reference/protocol#error-codes).
