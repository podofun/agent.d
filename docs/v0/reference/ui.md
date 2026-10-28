# UI Scripting Reference

`agentd.ui` lets the Lua in your configuration change what `agentctl chat` shows and how it reacts. You add elements such as text, buttons and boxes to a chat's screen, and change them from your handlers while the chat is open.

## Your first script

Put this in your `init.lua`, or in a file it imports:

```lua
agentd.ui.on("chat", function(chat)
  local doc = chat.document
  local label = doc:text({ text = chat.folder or "no folder", style = { fg = "muted" } })
  doc:get("header.right"):append(label)
end)
```

Open `agentctl chat`, and the chat's folder appears at the right end of the header.

The function you pass to `agentd.ui.on("chat", ...)` runs each time a chat opens. It receives the chat, and `chat.document` is where you create elements and find the ones already on screen. `doc:get("header.right")` finds a part of chat's screen by its id, and `append` puts your element inside it.

## Changing what you added

Keep the element in a variable and assign to its properties. The chat shows the change straight away.

A chat's folder changes when you open another session, so this version updates the label when that happens:

```lua
agentd.ui.on("chat", function(chat)
  local doc = chat.document
  local label = doc:text({ text = chat.folder or "no folder", style = { fg = "muted" } })
  doc:get("header.right"):append(label)

  chat:on("session", function(chat)
    label.text = chat.folder or "no folder"
  end)
end)
```

Styles and other properties change the same way:

```lua
label.style.fg = "accent"
label.style.bold = true
label.visible = false
```

Assigning `nil` to a property or a style removes it.

## Where you can put things

Chat's screen is made of parts that you find by id, as listed in [Chat's own parts](#chat-s-own-parts). Two of them are there for your elements:

- `header.right` is a row at the right end of the header. Elements you add there sit on the header's line, each as wide as its content.
- `main` is the row that holds the conversation. Elements you append to it appear to the right of the conversation, and elements you prepend appear to its left.

This puts a box with a border to the right of the conversation:

```lua
agentd.ui.on("chat", function(chat)
  local doc = chat.document
  doc:get("main"):append(doc:box({
    id = "notes",
    width = 30,
    style = { border = "rounded", title = "Notes" },
    children = {
      doc:text({ text = "Nothing yet." }),
    },
  }))
end)
```

The box is 30 cells wide, and the conversation takes the rest of the row. A cell is the width of one character.

### Sizing and arranging

A `box` stacks its children in a column. With `direction = "row"`, it lines them up side by side.

`width` and `height` take one of these:

| Size | Meaning |
|---|---|
| a number | That many cells |
| `"30%"` | That share of the parent's space |
| `"fill"` | The space left over, shared with the parent's other `fill` children |
| nothing | As much as the content needs |

In a row, `width` is the size that matters. In a column, `height` is. In the other direction, an element stretches across its parent unless you give it a size.

`padding` adds empty cells inside a box's edge, and `gap` adds empty cells between its children. A `spacer` is an empty element that fills the space it is given, which is useful for pushing the next element to the far end of a row.

### Scrolling

A box or a list with `scroll = true` can hold more than fits in it. The mouse wheel scrolls it, and a thin bar on its right shows where you are.

A scrolling box needs a size in its parent's direction, such as `height = "fill"` in a column. Without one, it grows to fit everything and has nothing to scroll.

## Reacting to clicks and typing

Give an element a handler for an event with `on`. This button counts its clicks:

```lua
agentd.ui.on("chat", function(chat)
  local doc = chat.document
  local count = 0
  local button = doc:button({ label = "Clicked 0 times" })

  button:on("click", function(event, chat, ctx)
    count = count + 1
    button.label = "Clicked " .. count .. " times"
  end)

  doc:get("header.right"):append(button)
end)
```

You can also pass handlers when you create the element, in an `on` table. This input copies what you type into the text below it:

```lua
local echo = doc:text({ text = "" })
local field = doc:input({
  placeholder = "Type here",
  on = {
    change = function(event)
      echo.text = event.value
    end,
  },
})
```

Every element handler receives the event, the chat and a `ctx`. When `change` arrives, the element already holds the new value, so `field.value` and `event.value` are the same.

If an element has no handler for an event, the event goes to its parent, and so on up. `event.target` is always the element where it started, so one handler on a box can serve every button inside it.

Clicking a button, checkbox, input, list or terminal gives it focus. A focused button or checkbox is clicked with Enter or Space, and a focused list moves its selection with Up and Down. Esc takes focus away, except from a terminal, and Ctrl+Q takes it away from anything.

See [Events](#events) for every event and what it carries.

## Keyboard shortcuts

`agentd.ui.bind` adds a shortcut to every chat. This one shows and hides the `notes` box from earlier:

```lua
agentd.ui.bind("ctrl+g", function(chat, ctx)
  local notes = chat.document:get("notes")
  if notes then
    notes.visible = not notes.visible
  end
end)
```

A shortcut you bind takes the key away from chat. Pick a key chat does not use: a plain letter, for example, would stop reaching the prompt.

Shortcuts do not fire while an input or a terminal has focus, since those take the keys you type. Ctrl+C, Shift+Tab and Enter always belong to chat, and so do the approval window's keys while it is open. See [Keys](#keys) for how to write key names.

## What a handler can do

A UI handler can do anything an action handler can. It can run a program with `ctx.shell`, call an action with `ctx.call`, run a runner, read files, and wait with `sleep`.

Its `ctx` acts for the chat. `ctx.caller` names the chat's interface, user and session, and relative paths start in the chat's folder:

```lua
agentd.ui.on("chat", function(chat)
  local doc = chat.document
  local listing = doc:text({ text = "" })
  local button = doc:button({ label = "List files" })

  button:on("click", function(event, chat, ctx)
    if not chat.folder then
      return chat:note("This chat has no folder yet.")
    end
    listing.text = ctx.shell("ls", {}).stdout
  end)

  doc:get("main"):append(doc:box({ width = 30, children = { button, listing } }))
end)
```

`chat.folder` is `nil` until the chat has a folder, so check it before you run something that depends on one.

UI handlers run without grants and without approval questions. Grants and approvals apply to what the agent does. See [Permissions](/v0/concepts/permissions).

To tell the person chatting something, call `chat:note(text)`. The note appears in the conversation.

Text is shown exactly as written, so terminal colour codes in a program's output appear as stray characters. Use `style` for colour.

## Changing conversation entries

The conversation is a list of entries: your messages, the agent's replies, its tool calls and chat's own notices. An `entry` handler runs for each entry a chat shows, and can change how it looks.

Setting `entry.summary` replaces the output of a closed tool row with one line. If your configuration has an action named `files.read` that takes a `path`, this shows the path:

```lua
agentd.ui.on("entry", function(entry, chat)
  if entry.name == "files.read" and entry.input and not entry.failed then
    entry.summary = entry.input.path
  end
end)
```

`entry.hidden = true` hides an entry, and `entry.style` restyles its text.

To show something above or below an entry, attach an element to it:

```lua
agentd.ui.on("entry", function(entry, chat)
  if entry.kind == "tool" and entry.failed then
    entry:detach()
    entry:attach_after(chat.document:text({ text = "This call failed.", style = { fg = "error" } }))
  end
end)
```

The handler runs again when an entry changes, such as when a tool call finishes. Call `entry:detach()` before you attach, or each run adds another copy.

Attach text, or boxes that hold text. Attached elements are shown as plain lines and do not react to clicks.

To change an entry later, keep its `entry.key` and look it up again with `chat:entry(key)`. Keys last until the chat opens another session.

See [Conversation entries](#conversation-entries) for every field.

## Animating

`chat:frames(fps, fn)` calls `fn` many times a second so you can animate elements. `fn` receives `t`, the time in seconds since the chat opened, and the chat.

This shows a spinner while a long command runs:

```lua
agentd.ui.on("chat", function(chat)
  local doc = chat.document
  local spinner = doc:text({ text = "", visible = false, style = { fg = "accent" } })
  local run = doc:button({ label = "Build" })
  doc:get("header.right"):append(spinner)
  doc:get("header.right"):append(run)

  run:on("click", function(event, chat, ctx)
    local marks = { "|", "/", "-", "\\" }
    spinner.visible = true
    local spin = chat:frames(10, function(t)
      spinner.text = marks[math.floor(t * 10) % #marks + 1]
    end)
    ctx.shell("make", {})
    spin:stop()
    spinner.visible = false
  end)
end)
```

A frame function has rules of its own:

- It can change the properties and styles of elements that already exist. Create everything it needs before you start it.
- It cannot create, move or remove elements, run programs, call actions or wait. Doing any of these stops it with an error.
- Work out what to show from `t` alone. `fn` can be called more than once for the same moment, so counting calls gives the wrong result.
- What it sets is only on screen. Reading the property elsewhere gives the value you set outside the animation, and that value comes back when you stop it.

`agentd.ui.frames(fps, fn)` animates every chat in the same way. Both take 1 to 60 frames per second and return a handle with `stop()`.

## Showing a program in a pane

A `terminal` element runs a program and shows its screen. Declare the program by name once, with its arguments as a list, then create a terminal element with that name:

```lua
agentd.ui.terminal("top", { "top" })

agentd.ui.on("chat", function(chat)
  local doc = chat.document
  doc:get("main"):append(doc:terminal({ name = "top", width = "40%" }))
end)
```

The program starts when the element is created, in the chat's folder, and stops when the element is destroyed or the chat closes. Click the pane to type into it, and press Ctrl+Q to leave it. Esc goes to the program.

A terminal with no size fills the space it is given. Programs run without grants or approval, like handlers.

## Making a whole screen

A view is a screen of its own, which the chat can switch to and back. Register one with a function that builds it and returns its outermost element:

```lua
agentd.ui.terminal("top", { "top" })

agentd.ui.view("monitor", function(chat, ctx)
  local doc = chat.document
  return doc:box({
    children = {
      doc:text({ text = "Press F2 to go back", style = { fg = "muted" } }),
      doc:terminal({ name = "top" }),
    },
  })
end)

agentd.ui.bind("f2", function(chat)
  chat.view = chat.view == "chat" and "monitor" or "chat"
end)
```

Chat's usual screen is the view named `chat`. Assign a view's name to `chat.view`, or call `chat:show(name)`, to switch.

A chat builds a view the first time it shows it, and keeps it afterwards, so the function runs once per chat.

## Reusing pieces

A component is an element you build from other elements and use by name. Register it with a function that receives the properties it was given and the chat, and returns an element:

```lua
agentd.ui.component("Card", function(props, chat)
  local doc = chat.document
  local card = doc:box({ padding = 1, style = { border = "rounded", title = props.title } })
  for _, child in ipairs(props.children or {}) do
    card:append(child)
  end
  return card
end)
```

Create it like any element, from the document:

```lua
local card = doc:Card({ title = "Status", children = { doc:text({ text = "All good" }) } })
```

A component receives every property as you passed it, including `id`, `width` and `on`. It is up to your function to use them. A component cannot be named after a built-in element, such as `box` or `text`.

## Showing a list from data

A list can draw its items from a table. Give it the table as `items`, and a `render` function that returns an element for one item:

```lua
local names = agentd.ui.data({
  { id = "ada", text = "Ada" },
  { id = "alan", text = "Alan" },
})

local people = doc:list({
  items = names,
  render = function(item, index, chat)
    return chat.document:text({ text = item.text })
  end,
})
```

Wrapping the table in `agentd.ui.data` makes the list redraw when you write to the table:

```lua
table.insert(names, { id = "grace", text = "Grace" })
names[1] = { id = "ada", text = "Ada L." }
```

Changing a field inside an item does not count as a write to the table. After `names[1].text = "Ada"`, call `people:refresh()`.

Give each item an `id` (or a `key`), and keep it when you replace the item, so the list knows which item is which.

Assigning another table to `people.items` shows that table instead. Clicking an item selects it, and `people.selected` is its position, counting from 1.

## What scripts cannot change

A few parts of chat stay as they are, whatever a script does:

- The prompt cannot be hidden, removed or destroyed. You can still move it and restyle it.
- The approval window always appears when the agent asks for approval, and its keys always work.
- Ctrl+C, Shift+Tab and Enter cannot be bound.
- You cannot register a view named `chat`. To change chat's usual screen, change its parts.

`destroy` does nothing on chat's own parts. To take one off the screen, hide it with `visible = false`.

## When something goes wrong

When a handler fails, the chat shows the error in the conversation, starting with "A UI script failed.", and the same line appears in agentd's output.

When a terminal's program cannot start, the note starts with "A terminal did not start." instead. Check that the program is installed and on the `PATH` agentd runs with.

A mistake in a call at the top level of your configuration, such as binding Ctrl+C, stops the configuration from loading, like any other error in it.

These are the errors you are most likely to meet:

| Error | What to do |
|---|---|
| `` `ctrl+c` belongs to chat, so scripts cannot bind it `` | Pick another key |
| ``there is no terminal named `x`, declare it with agentd.ui.terminal first`` | Call `agentd.ui.terminal("x", ...)` at the top level of your configuration |
| ``there is no element or component named `x` `` | Check the spelling, and register the component before you use it |
| `a frame function cannot create elements, so create them before you start the animation` | Create and place the elements before you call `frames` |
| `a frame function can only change properties and styles, so move this call out of it` | Move the call, such as `ctx.shell` or `sleep`, out of the frame function |
| `an element cannot be stored as a property, append it instead` | Use `append` to put an element inside another |
| `that element belongs to another chat` | Create the element from this chat's `chat.document` |
| ``a list bound to items needs a `render` function`` | Pass `render` together with `items` |
| `the prompt belongs to chat, so scripts cannot hide it` | Leave the prompt visible, see [What scripts cannot change](#what-scripts-cannot-change) |

## Reloading your scripts

Start agentd with `--watch` while you work on your scripts. Each time you save, every open chat goes back to its usual screen and your `chat` handlers run for it again.

Without `--watch`, restart agentd and open `agentctl chat` again.

## Reference

### `agentd.ui`

Register handlers, shortcuts, views, components and terminals at the top level of your configuration.

| Function | What it does |
|---|---|
| `agentd.ui.on(event, fn)` | Runs `fn` for every chat when `event` happens, see below |
| `agentd.ui.bind(key, fn)` | Adds a keyboard shortcut to every chat |
| `agentd.ui.view(name, fn)` | Registers a view that `fn` builds |
| `agentd.ui.component(name, fn)` | Registers a component that `fn` builds |
| `agentd.ui.terminal(name, argv)` | Declares a program that `terminal` elements can run |
| `agentd.ui.frames(fps, fn)` | Animates every chat, see [Animating](#animating) |
| `agentd.ui.data(table)` | Wraps a table so the lists showing it redraw when it changes |
| `agentd.ui.chats()` | Lists the open chats |

`agentd.ui.on` takes these events:

| Event | When |
|---|---|
| `chat` | A chat opens, and again for every open chat after a reload |
| `session` | A chat opens another session, or starts a new one |
| `entry` | A chat shows a conversation entry, or one changes |

### What your functions receive

| Function | Arguments |
|---|---|
| `agentd.ui.on("chat", fn)`, `agentd.ui.on("session", fn)` | `chat`, `ctx` |
| `agentd.ui.on("entry", fn)` | `entry`, `chat`, `ctx` |
| An element's handler | `event`, `chat`, `ctx` |
| `agentd.ui.bind(key, fn)` | `chat`, `ctx` |
| `agentd.ui.view(name, fn)` | `chat`, `ctx` |
| `agentd.ui.component(name, fn)` | `props`, `chat` |
| A list's `render` | `item`, `index`, `chat` |
| A frame function | `t`, `chat` |

### Elements

| Element | What it shows | Its own properties |
|---|---|---|
| `box` | Other elements, in a column or a row | `direction`, `padding`, `gap`, `scroll` |
| `text` | Text, wrapped to its width | `text` |
| `button` | A label to click, drawn as `[ Label ]` | `label` |
| `input` | A one-line text field | `value`, `placeholder` |
| `checkbox` | A box to tick, drawn as `[x]` or `[ ]` | `label`, `checked` |
| `list` | Items in a column, one of them selected | `selected`, `items`, `render`, and the same as `box` |
| `terminal` | A program's screen | `name` |
| `spacer` | Empty space that grows | |

Create an element with the document method of the same name, such as `doc:box(props)`, or with `doc:create("box", props)`.

`direction` is `"column"` (the default) or `"row"`. `padding` and `gap` are numbers of cells. `selected` is a position counting from 1.

Every element also takes these properties:

| Property | Meaning |
|---|---|
| `id` | A name to find the element by, with `doc:get` or `el:find` |
| `visible` | `false` hides it and gives its space to the others |
| `width`, `height` | Its size, see [Sizing and arranging](#sizing-and-arranging) |
| `style` | Its colours, text style and border, see [Styles](#styles) |
| `children` | Elements to put inside it |
| `on` | Handlers, by event name |

### Element fields and methods

| Field or method | What it does |
|---|---|
| `el:append(child)`, `el:prepend(child)` | Puts `child` inside, at the end or the start |
| `el:insert(child, index)` | Puts `child` inside at a position, counting from 1. An element already inside moves there |
| `el:remove()` | Takes it out of its parent. You can put it back later |
| `el:destroy()` | Removes it for good, with everything inside it, and stops any program in it |
| `el:on(event, fn)` | Adds a handler |
| `el:off(event)` | Removes its handlers for `event` |
| `el:find(id)` | The first element inside it with that `id` |
| `el:refresh()` | Redraws a list after you changed one of its items |
| `el.kind` | The element's name, such as `box` |
| `el.parent` | The element it is inside, or `nil` |
| `el.children` | The elements inside it, as a list |

`kind`, `parent` and `children` can be read but not assigned. Every other property can be read and assigned.

Assigning a table to `style`, as in `el.style = { fg = "red" }`, sets those styles and leaves the others as they were.

### Styles

| Style | Values |
|---|---|
| `fg` | The text colour |
| `bg` | The background colour |
| `bold`, `italic`, `underline`, `dim` | `true` or `false` |
| `border` | `"none"`, `"line"`, `"rounded"`, `"double"` or `"thick"` |
| `border_fg` | The border colour. Without it, the border takes `fg`, or else `muted` |
| `title` | Text set into the top border. It shows only on an element with a border |

A colour is one of these:

- a theme colour, which follows chat's own colours: `accent`, `secondary`, `muted`, `success`, `warning`, `error`, or `text` for the normal text colour;
- a name: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `darkgray` or `white`;
- a hex value, such as `"#ff8800"`;
- `"@n"` for colour `n` of the terminal's 256 colours, such as `"@208"`.

### Events

| Event | When | Event fields |
|---|---|---|
| `click` | The element is clicked, or Enter or Space is pressed while it has focus. Enter on a focused list clicks its selected item | `target` |
| `change` | An input's text, a checkbox's tick or a list's selection changes | `target`, `value` |
| `key` | A key other than typing, such as `enter`, `tab` or `up`, is pressed in a focused input | `target`, `key` |
| `focus`, `blur` | The element gains or loses focus | `target` |
| `hover` | The mouse moves onto the element | `target` |

Every event also has `type`, the event's name.

A chat raises events of its own, which you handle with `chat:on(event, fn)`:

| Event | When | Handler receives |
|---|---|---|
| `session` | The chat opens another session, or starts a new one | `chat`, `ctx` |
| `entry` | The chat shows an entry, or one changes | `entry`, `chat`, `ctx` |
| `key` | A shortcut bound with `agentd.ui.bind` is pressed. `event.key` names it | `event`, `chat`, `ctx` |
| `resize` | The chat's window changes size. `event.width` and `event.height` give the new size | `event`, `chat`, `ctx` |

### Keys

Key names are lower case, with modifiers first in the order `ctrl`, `alt`, `shift`: `ctrl+g`, `alt+x`, `ctrl+shift+k`. `agentd.ui.bind` accepts other spellings, such as `Ctrl+G`, and writes them this way.

Besides letters, digits and symbols, you can use `f1` to `f12`, `enter`, `escape`, `tab`, `backspace`, `delete`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup` and `pagedown`. An upper-case letter is `shift` with the letter, such as `shift+g`.

### Chat's own parts

| Id | What it is |
|---|---|
| `chat` | The whole screen, a column holding everything below |
| `header` | The line with the chat's name and state |
| `header.right` | A row at the right end of the header, empty until you add to it |
| `main` | The row that holds the conversation |
| `conversation` | The conversation |
| `prompt` | Where you type your message |
| `status` | The line with the folder, branch, runner and approval mode |

You can restyle, move and hide these like your own elements, except for the limits in [What scripts cannot change](#what-scripts-cannot-change).

### Conversation entries

| Field | Meaning |
|---|---|
| `entry.key` | Identifies the entry until the chat opens another session |
| `entry.kind` | `user`, `agent`, `tool`, `info`, `error` or `setting` |
| `entry.text` | The message, on every entry except a tool call |
| `entry.name` | On a tool call: the action or tool it called |
| `entry.input` | On a tool call: the arguments, as a table |
| `entry.result` | On a tool call: what it returned |
| `entry.failed` | `true` when the tool call failed |
| `entry.changes` | The files the tool call changed, each with `path`, `operation`, `added` and `removed` |
| `entry.summary` | One line shown on a closed tool row in place of its output. You can set it |
| `entry.hidden` | `true` hides the entry. You can set it |
| `entry.style` | The style of the entry's text. You can set it |

| Method | What it does |
|---|---|
| `entry:attach_before(el)`, `entry:attach_after(el)` | Shows an element above or below the entry |
| `entry:detach()` | Removes every element attached to the entry |

### Chats

| Field or method | Meaning |
|---|---|
| `chat.id` | A number that identifies the chat while it is open |
| `chat.session` | The id of the session it shows, or `nil` for a new chat |
| `chat.folder` | The folder it works in, or `nil` until it has one |
| `chat.user`, `chat.interface` | Who is chatting, and through which interface |
| `chat.client` | The window: `kind` (`tui` for `agentctl chat`), `width` and `height` in cells |
| `chat.document` | Where you create and find elements |
| `chat.view` | The name of the view it shows. Assign to switch |
| `chat:show(name)` | Switches to a view |
| `chat:on(event, fn)` | Handles one of the chat's own [events](#events) |
| `chat:frames(fps, fn)` | Animates this chat, see [Animating](#animating) |
| `chat:note(text)` | Shows a note in the conversation |
| `chat:entry(key)` | The entry with that key, or `nil` |

### The document

| Field or method | What it does |
|---|---|
| `doc:box(props)`, `doc:text(props)`, and so on | Creates an element, one method per element |
| `doc:Name(props)` | Creates the component registered as `Name` |
| `doc:create(name, props)` | Creates an element or a component by name |
| `doc:get(id)` | The element with that `id`, wherever it is |
| `doc.root` | The outermost element of the view on screen |

An element you create stays off screen until you put it inside one that is shown.

Writing your own chat client? See the [UI protocol](/v0/reference/ui-protocol).
