# UI Scripting Reference

`agentd.ui` lets your configuration build what a chat shows, much like a web page is built from elements. A script can put a panel beside the conversation, restyle chat's own parts, summarize tool rows, animate the prompt border, or open lazygit next to the agent's work.

All of this runs in the daemon. A chat client such as `agentctl chat` runs no script code: it draws what the daemon sends and reports what you do back to it. Because handlers run where everything else lives, a button can call `ctx.shell`, an action, memory or a runner, exactly like an action handler can. The same scripts will drive other clients, such as a web chat, since they describe what to show and leave the drawing to the client.

The second half of this page documents the protocol between the daemon and a client, for anyone writing their own client.

## A first panel

Each chat that connects runs your `ui.on("chat")` handlers. This one puts a small panel to the right of the conversation, with a button that runs `git status`:

```lua
local ui = agentd.ui

ui.on("chat", function(chat, ctx)
  local doc = chat.document
  local output = doc:text({ text = "Press the button", style = { fg = "muted" } })
  local button = doc:button({ label = "git status" })
  button:on("click", function(event, chat, ctx)
    local result = ctx.shell("git", { "status", "--short" })
    output.text = result.stdout
  end)
  local panel = doc:box({
    width = 40,
    style = { border = "rounded", title = "Git" },
    children = { button, output },
  })
  doc:get("main"):append(panel)
end)
```

The button's program needs a grant, like any `ctx.shell` call. UI handlers use the grants in `[tool.ui]`, described under [Permissions](#permissions).

## Chats and documents

A chat is one connected client window. Your handlers receive it as `chat`:

| Field or method | Description |
|---|---|
| `chat.id` | A number that identifies the chat while it is open |
| `chat.session` | The id of the session it shows, or `nil` for a new chat |
| `chat.folder` | The folder the chat works in, once it has one |
| `chat.user`, `chat.interface` | Who is chatting, and through which interface |
| `chat.client` | `kind` (`tui` for `agentctl chat`), `width`, `height`, `extras`, and `supports(name)` |
| `chat.document` | The chat's elements |
| `chat.view` | The name of the view it shows; assign to switch |
| `chat:on(event, fn)` | Listen for `session`, `entry`, `key` or `resize` on this chat |
| `chat:show(name)` | Switch to a view |
| `chat:frames(fps, fn)` | Animate this chat, see [Frames](#frames) |
| `chat:note(text)` | Show a short note to the user |
| `chat:entry(key)` | A conversation entry, see [Conversation entries](#conversation-entries) |

`agentd.ui.chats()` lists the chats that are open, and `agentd.ui.on` listens across all of them:

| Event | Handler | When |
|---|---|---|
| `chat` | `fn(chat, ctx)` | A chat connects, and again for every open chat after a hot reload |
| `session` | `fn(chat, ctx)` | A chat opens another session or starts a new one |
| `entry` | `fn(entry, chat, ctx)` | A chat shows a conversation entry, including when it reopens a session |

The `ctx` a handler receives acts for that chat: `ctx.caller` names its interface, user and session, relative paths start in the chat's folder, and `{cwd}` grants point at that folder.

## Elements

The document creates elements. Each core element has a method of its own, and `doc:create(kind, props)` makes any element or component by name:

| Element | What it shows | Main properties |
|---|---|---|
| `box` | A row or column of other elements | `direction`, `padding`, `gap` |
| `text` | Text, wrapped to its width | `text` |
| `button` | Something to click, drawn as `[ Label ]` in the terminal | `label` |
| `input` | A one-line text field | `value`, `placeholder` |
| `list` | A column of items, one of them selected | `selected`, or `items` and `render` |
| `checkbox` | A box to tick | `label`, `checked` |
| `terminal` | A program's screen, see [Terminal panes](#terminal-panes) | `name` |
| `spacer` | Empty space that grows | |

Every element also takes `id`, `visible`, `width`, `height` and `style`, plus `children` (a list of elements) and `on` (a table of event handlers):

```lua
local doc = chat.document
local box = doc:box({
  id = "notes",
  direction = "row",
  gap = 1,
  children = { doc:text({ text = "Notes" }), doc:spacer() },
  on = { click = function(event, chat, ctx) end },
})
```

Read and change an element by assigning to it, as you would a table:

```lua
box.visible = false
note.text = "Saved"
note.style.fg = "success"
```

Elements have these methods:

| Method | Description |
|---|---|
| `el:append(child)`, `el:prepend(child)` | Add a child at the end or the start |
| `el:insert(child, index)` | Add or move a child to a position, counting from 1 |
| `el:remove()` | Take it out of its parent; it can be added again |
| `el:destroy()` | Remove it for good, with everything inside it |
| `el:on(event, fn)`, `el:off(event)` | Listen for an event, or stop |
| `el:find(id)` | The first element inside with that `id` |
| `el.parent`, `el.children`, `el.kind` | Where it sits and what it is |

`doc:get(id)` finds an element anywhere in the document, and `doc.root` is the root of the current view.

Extra elements that only some clients draw have a dotted name, such as `web.editor`. Check `chat.client.supports("web.editor")` before you create one. A client skips elements and properties it does not know, so a script keeps working on clients that lack them.

## Chat's own parts

The screen chat draws by default is itself a view named `chat`, built from elements with fixed ids:

| Id | What it is |
|---|---|
| `chat` | The root, a column |
| `header` | The line with the chat's name and notices |
| `main` | A row that holds the conversation; add panels here |
| `conversation` | The conversation |
| `prompt` | Where you type |
| `status` | Folder, branch, runner and approval mode |

Scripts can restyle, wrap and move these parts like any element. With no UI script, chat looks exactly as it always has.

## Layout

A `box` lays its children out in a column, or in a row with `direction = "row"`. Sizes are measured in cells, about one character wide (on the web, `ch`):

| Size | Meaning |
|---|---|
| a number | That many cells |
| `"30%"` | A share of the space in the parent |
| `"fill"` | What is left, shared with the other `fill` children |
| nothing | What the content needs |

`width` sizes an element across a row and `height` down a column. Across the other direction, an element stretches to fill its parent unless it has a size. `padding` adds space inside a box and `gap` adds space between its children, both in cells. Spacers and terminals fill the space they are given when they have no size.

## Style

Styles go in `style` and change by assignment, `el.style.bold = true`:

| Style | Values |
|---|---|
| `fg`, `bg` | A colour |
| `bold`, `italic`, `underline`, `dim` | `true` or `false` |
| `border` | `"none"`, `"line"` or `"rounded"` |
| `border_fg` | The colour of the border |
| `title` | Text set into the top border |

A colour is a name (`red`, `cyan`, `gray`), a hex value (`"#ff8800"`), or a theme token that follows the client's theme: `accent`, `secondary`, `muted`, `success`, `warning`, `error` and `text`.

## Events

Elements raise these events. A handler receives the event, the chat and a `ctx`:

| Event | When | Fields |
|---|---|---|
| `click` | The element is clicked, or Enter or Space while it has focus | `target` |
| `key` | A key while the element has focus | `key`, such as `enter` or `ctrl+g` |
| `change` | An input, checkbox or list selection changes | `value` |
| `focus`, `blur` | The element gains or loses focus | |
| `hover` | The mouse moves onto the element | |

An event goes to the nearest element that listens, starting from the one under the mouse. The client handles typing in an input, scrolling and selecting text by itself, and tells the script with `change` when a value changes.

`ui.bind(key, fn)` binds a shortcut in every chat, including keys chat already uses such as `ctrl+e`:

```lua
agentd.ui.bind("ctrl+g", function(chat, ctx)
  chat.view = chat.view == "chat" and "workspace" or "chat"
end)
```

Some keys stay with chat whatever a script does: Ctrl+C, the keys of an approval window, Shift+Tab for the approval mode, and Enter to send. Binding one of them is an error, and scripts cannot hide the prompt or the approval window.

## Views

A view is a whole screen. Register one with a function that builds its root element, then switch a chat to it:

```lua
agentd.ui.view("workspace", function(chat, ctx)
  local doc = chat.document
  return doc:box({
    direction = "row",
    children = { doc:text({ text = "Files", width = 30 }), doc:terminal({ name = "lazygit" }) },
  })
end)

-- later, in a handler
chat:show("workspace")
```

The view is built the first time a chat shows it and kept afterwards. `chat.view = "chat"` goes back to the conversation, with the draft as you left it.

## Components

A component is a reusable element made of other elements. Register it with a function that receives its properties and the chat, and use it by name:

```lua
agentd.ui.component("Section", function(props, chat)
  local doc = chat.document
  local box = doc:box({ style = { border = "rounded", title = props.title } })
  for _, child in ipairs(props.children or {}) do
    box:append(child)
  end
  return box
end)

local section = chat.document:Section({ title = "Tasks", children = { list } })
```

## Lists bound to data

A list can draw a table of items with a `render` function. Wrap the table in `ui.data` and every change to it redraws the list, rendering only the items that are new or changed:

```lua
local tasks = agentd.ui.data({ { id = 1, text = "Read the code" } })
local list = doc:list({
  items = tasks,
  render = function(item, index, chat)
    return chat.document:text({ text = item.text })
  end,
})

table.insert(tasks, { id = 2, text = "Write tests" })   -- one new item is rendered
tasks[1] = { id = 1, text = "Read the code again" }     -- only the first is rendered again
```

An item's `id` (or `key`) tells items apart, so keep it the same when you replace an item. Assigning `list.items = other` binds another table, and `list:refresh()` redraws after you change an item in place.

## Conversation entries

Chat keeps the conversation itself. Scripts can read each entry and change how it is shown, and hear about every entry through `entry` events, including the ones a reopened chat shows:

| Field | Description |
|---|---|
| `entry.key` | Identifies the entry until the chat switches sessions |
| `entry.kind` | `user`, `agent`, `tool`, `info`, `error` or `setting` |
| `entry.text` | The message text, for everything but tool rows |
| `entry.name`, `entry.input`, `entry.result` | A tool row's action, arguments and result |
| `entry.failed`, `entry.changes` | Whether the call failed, and the files it changed |
| `entry.summary` | One line shown on a closed tool row in place of its output |
| `entry.hidden` | Hide the entry |
| `entry.style` | Style the entry's text |

`entry:attach_before(el)` and `entry:attach_after(el)` show an element above or below the entry, and `entry:detach()` removes them. Scripts change how an entry looks; its text stays as it was written.

```lua
agentd.ui.on("entry", function(entry, chat)
  if entry.name == "todo.write" then
    local done = 0
    for _, item in ipairs(entry.input.items) do
      if item.status == "done" then done = done + 1 end
    end
    entry.summary = string.format("%d of %d done", done, #entry.input.items)
  end
end)
```

## Frames

`chat:frames(fps, fn)` animates a chat. agent.d calls `fn(t, chat)` with the seconds since the chat connected, and draws a few hundred milliseconds ahead so the client can play the frames smoothly. `ui.frames(fps, fn)` does the same for every chat. Both return a handle with `stop()`:

```lua
local frames = chat:frames(20, function(t)
  chat.document:get("prompt").style.border_fg = (math.floor(t) % 2 == 0) and "accent" or "muted"
end)
frames:stop()
```

Because frames are drawn ahead of time, a frame function may only change elements that already exist. Anything else, such as `ctx.shell`, `sleep` or creating an element, is an error. When a frame function fails, it stops and the chat shows a note.

## Terminal panes

A terminal pane shows a program, such as lazygit or htop, beside the conversation. Declare the program by name in your configuration, then show it with a `terminal` element:

```lua
agentd.ui.terminal("lazygit", { "lazygit" })

agentd.ui.on("chat", function(chat)
  local doc = chat.document
  doc:get("main"):append(doc:terminal({ name = "lazygit", width = "50%" }))
end)
```

The program starts in the chat's folder, on the daemon's machine, in the same sandbox as `ctx.shell`, with the same denials and approval questions, and keeps running until its element is destroyed or the chat closes. A chat that has not chosen a folder yet opens its terminals after the first message. Clients receive the screen as a grid of characters and colours, so a program's output reaches your terminal only as text. Click a pane to type into it, and press Ctrl+Q to leave it.

Terminal panes run on Linux and macOS. A pane cannot reach the network inside the sandbox, so a program that needs a `net:` grant is refused with a note.

## Permissions

Using `agentd.ui` needs no grant. What handlers and terminal panes do with `ctx` uses the grants in `[tool.ui]`, where `{cwd}` is the chat's folder:

```toml
[tool.ui]
granted = ["shell.exec:git", "shell.exec:lazygit", "fs.read:{cwd}/**", "fs.write:{cwd}/**"]
```

Anything else asks you first, as it would for an action, and "allow from now on" adds it to `[tool.ui]`.

## Errors and hot reload

When a handler fails, the chat shows a short note and the daemon logs the error; the rest of the UI keeps working. With `agentd --watch`, saving a script clears every chat's UI back to the default view and runs your scripts again. The conversation and the draft stay as they were.

---

## The protocol

Everything travels over the `/ws` WebSocket described in the [protocol reference](/v0/reference/protocol), with the same envelopes and tokens. This part is for people writing a client. The protocol version is `1`.

### Subscribing

A client opens a chat with `ui.subscribe`, a streaming request that stays open while the chat is:

```json
{ "id": 1, "method": "ui.subscribe", "params": {
  "version": 1,
  "client": { "kind": "tui", "width": 120, "height": 40, "supports": [] },
  "session": "4f1c…",
  "folder": "/home/me/project"
} }
```

| Param | Description |
|---|---|
| `version` | The protocol version the client speaks |
| `client` | `kind`, `width` and `height` in cells, and the extra elements it draws in `supports` |
| `session` | The session the chat shows, if any |
| `folder` | The folder the client runs in; used when the user trusts it |
| `user` | The end user, for interfaces that carry their own users |

The daemon answers with a `ui.ready` event, then a `ui.frame` event for every frame:

```json
{ "event": "ui.ready", "id": 1, "ready": { "version": 1, "chat": 7, "clock": 81234 } }
{ "event": "ui.frame", "id": 1, "frame": { "seq": 1, "at": 81234, "reset": true, "ops": [ … ] } }
```

`clock` is the daemon's time in milliseconds. Note it with your own clock when `ui.ready` arrives, and use the pair to tell when each frame is due. Cancel the request with `runners.cancel`, or close the socket, to close the chat.

### Frames

Every change to a chat lands on a frame, and all changes in a frame arrive as one message:

| Field | Description |
|---|---|
| `seq` | Increases with every frame |
| `at` | When to show the frame, on the daemon's clock |
| `reset` | Drop the whole UI first: the ops rebuild it |
| `cancel_ahead` | Drop frames received earlier for a time after `at` |
| `ops` | The changes, in order |

Most frames are due at once. Frames from frame functions arrive up to about 300 milliseconds early, stamped with their time, so play them from a queue on your own loop. An event from the user replaces animation already sent, and the frame that follows carries `cancel_ahead`. When several frames are due together, apply them all and draw once.

The first frame always has `reset`, and so does the frame after a hot reload. After a reset, send your conversation entries again.

### Ops

Each op has an `op` field:

| Op | Fields | Meaning |
|---|---|---|
| `create` | `node`, `kind`, `props`, `style` | A new element, not yet in the tree |
| `set` | `node`, `key`, `value` | Set a property; `null` removes it |
| `style` | `node`, `key`, `value` | Set a style; `null` removes it |
| `insert` | `parent`, `node`, `index` | Put an element under a parent at an index from 0, moving it if needed |
| `remove` | `node` | Take an element out of its parent |
| `destroy` | `node` | Forget an element and everything under it |
| `root` | `node` | Show the tree under this element |
| `bindings` | `keys` | The shortcuts scripts bound, such as `ctrl+g` |
| `entry` | `key`, `decoration` | How to show a conversation entry: `summary`, `hidden`, `style`, `before` and `after` |
| `screen` | `node`, `screen` | A terminal element's screen: `width`, `height`, `lines` of styled runs, and `cursor` |
| `note` | `text` | A message to show the user |

Chat's own parts have the fixed node ids 1 to 6: `chat`, `header`, `main`, `conversation`, `prompt` and `status`. Script elements start at 100. In a terminal screen, a colour like `@3` is an index in the terminal's 256-colour palette.

Treat every string as text to draw. Replace control characters before you show them, so a script cannot send escape sequences to the user's terminal.

### What the client sends

Each of these is an ordinary request on the same connection:

| Method | Params | When |
|---|---|---|
| `ui.event` | `chat`, `node?`, `type`, and the event's fields | The user clicked, typed, focused, hovered, or resized |
| `ui.session` | `chat`, `session?` | The chat opened another session, or a new chat |
| `ui.entry` | `chat`, `entry` | The chat shows a conversation entry, or one changed |

Event types are `click`, `key` (with `key`), `change` (with `value`), `focus`, `blur`, `hover`, `input` (with `data`, the bytes for a terminal element) and `resize` (with `width` and `height`). A `resize` without a `node` is the whole window; with a terminal's `node`, it is the size of that pane. A `key` without a `node` is a bound shortcut.

An entry has a `key` the client chooses, `kind`, and for tool rows `name`, `input`, `result`, `failed` and `changes`. Send it when it first appears and whenever it changes.

A client keeps Ctrl+C, approval windows and their keys, approval mode switching and sending the prompt for itself, whatever the scripts bound.
