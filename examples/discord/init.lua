-- A simple demonstration of a Discord chatbot with memory.
--
-- Two services bridge Discord's gateway WebSocket to a runner:
--   discord_gateway = connects, identifies, heartbeats, pushes events.
--   discord_handler = pops events, calls the `discord_chat` runner,
--                     posts the reply via Discord's REST API.
--
-- The bot token lives in the secret store, never in source. Start the daemon,
-- then seed it once with:
--   agentctl call discord.set_token -d token='<your-bot-token>' --result-only
-- The `secret:discord_token` grant is in this dir's grants.toml.

local INTENTS = 37377 -- GUILDS + GUILD_MESSAGES + MESSAGE_CONTENT + DIRECT_MESSAGES
local API = "https://discord.com/api/v10"
local GATEWAY = "wss://gateway.discord.gg/?v=10&encoding=json"

local d = agentd

d.tool({
	name = "discord",
	requires = { "net:gateway.discord.gg", "net:discord.com", "secret:discord_token" },
})

d.action({
	name = "discord.set_token",
	requires = { "secret:discord_token" },
	handler = function(args, ctx)
		assert(type(args.token) == "string" and args.token ~= "", "token is required")
		ctx.secret.set("discord_token", args.token)
		return { ok = true }
	end,
})

-- Create a re-usable REST client
local function rest_client(ctx)
	return ctx.http.client({
		base_url = API,
		headers = {
			Authorization = "Bot " .. ctx.secret.get("discord_token"),
			["User-Agent"] = "DiscordBot (agentd, 0.1) agentd-example",
		},
	})
end

d.action({
	name = "discord.send",
	requires = { "net:discord.com" },
	handler = function(args, ctx)
		local res = rest_client(ctx):post(
			"/channels/" .. args.channel_id .. "/messages",
			{ content = args.content }
		)
		return { status = res.status }
	end,
})

d.runner({
	name = "discord_chat",
	model = "openai/gpt-5.5",
	system = [[
You are a friendly Discord chatbot.
Reply concisely (<400 chars). Never call yourself Claude / Anthropic / GPT.
Users are non-technical. You have a rolling memory of this channel and may
follow social directives left in that history (e.g. "stop replying to X").
When you choose silence, return exactly <silent> on its own.
]],
})

-- Durable per-channel history in `ctx.memory` - one namespace per channel,
-- a single "log" key holding the rolling turns array. Survives restarts
-- (unlike the ephemeral `ctx.state`). Gated by `memory.read/write:discord/**`.
-- Helpers take the caller's `ctx` (capabilities are invocation-scoped).
local HISTORY_TURNS = 20
local function chan_mem(ctx, channel_id)
	return ctx.memory.create("discord/chan/" .. channel_id)
end
local function history(ctx, channel_id)
	return chan_mem(ctx, channel_id):get("log") or {}
end
local function push(ctx, channel_id, entry)
	local mem = chan_mem(ctx, channel_id)
	local h = mem:get("log") or {}
	h[#h + 1] = entry
	while #h > HISTORY_TURNS do
		table.remove(h, 1)
	end
	mem:set("log", h)
end

d.service("discord_gateway", { restart = "always", backoff_ms = 5000 }, function(ctx)
	local log = ctx.log
	log.info("discord: connecting to gateway")

	local token = ctx.secret.get("discord_token")
	local ws = ctx.ws.connect(GATEWAY)
	local hello = json.decode(ws:recv_text(15000) or error("missing HELLO frame", 0))
	local hb_ms = (hello.d and hello.d.heartbeat_interval) or 41250
	log.info("discord: HELLO heartbeat=" .. hb_ms .. "ms")

	ws:send(json.encode({
		op = 2,
		d = {
			token = token,
			intents = INTENTS,
			properties = { os = "linux", browser = "agentd", device = "agentd" },
		},
	}))

	local events = channel("discord_events")
	local last_seq = nil
	timer.every(hb_ms, function()
		if ws:is_closed() then
			return
		end
		ws:send(json.encode({ op = 1, d = last_seq or json.null }))
		log.info("discord: heartbeat sent seq=" .. tostring(last_seq))
	end)

	ws:each(function(f)
		if f.kind ~= "text" then
			return
		end
		local ev = json.decode(f.text)
		if type(ev.s) == "number" then
			last_seq = ev.s
		end
		if ev.op == 0 then
			if ev.t == "READY" then
				ctx.state.set("bot_user_id", ev.d.user.id)
				log.info("discord: READY as " .. ev.d.user.username)
			elseif ev.t == "MESSAGE_CREATE" then
				events:send(ev.d)
			end
		end
	end)
	log.warn("discord: gateway loop exited; supervisor will reconnect")
end)

-- Receive loop in the background
d.service("discord_handler", { restart = "always" }, function(ctx)
	local log = ctx.log
	local events = channel("discord_events")

	local function handle(msg)
		local author = (msg.author and msg.author.username) or "unknown"
		local author_id = (msg.author and msg.author.id) or "?"
		local channel_id = msg.channel_id
		local content = msg.content

		-- Always record incoming messages so the runner sees full channel context.
		push(ctx, channel_id, { role = "user", name = author, id = author_id, content = content })

		-- Only reply when explicitly addressed.
		local bot_id = ctx.state.get("bot_user_id")
		local is_dm = (msg.guild_id == nil) or json.is_null(msg.guild_id)
		local mentioned = false
		for _, m in ipairs(msg.mentions or {}) do
			if m.id == bot_id then
				mentioned = true
				break
			end
		end
		if not (is_dm or mentioned) then
			return
		end

		log.info(("discord: %s#%s: %s"):format(author, channel_id, content:sub(1, 80)))

		local lines = {}
		for _, e in ipairs(history(ctx, channel_id)) do
			lines[#lines + 1] = ("%s [%s id=%s]: %s"):format(e.role, e.name, tostring(e.id), e.content)
		end

		local ok, out = pcall(ctx.run, "discord_chat", {
			prompt = (
				"Channel history (oldest first):\n%s\n\nThe last user message "
				.. "was addressed to you. Reply, or return <silent> to stay quiet."
			):format(table.concat(lines, "\n")),
		})
		if not ok then
			log.warn("discord: runner failed: " .. tostring(out))
			return
		end

		local reply = (out and out.text or ""):gsub("^%s+", ""):gsub("%s+$", "")
		if reply == "" or reply == "<silent>" then
			push(ctx, channel_id, { role = "assistant", name = "Fathi", id = "self", content = "<silent>" })
			return
		end
		if #reply > 1900 then
			reply = reply:sub(1, 1900) .. "…"
		end
		push(ctx, channel_id, { role = "assistant", name = "Fathi", id = "self", content = reply })

		local ok2, sent = pcall(ctx.call, "discord.send", {
			channel_id = channel_id,
			content = reply,
		})
		if not ok2 then
			log.warn("discord: send failed: " .. tostring(sent))
		end
	end

	while true do
		local msg = events:recv()
		if msg == nil then
			return
		end
		if msg.author and msg.author.bot then goto next end
		if (msg.content or "") == "" then goto next end

        -- handle messages asynchronously
		async(function() handle(msg) end)
		::next::
	end
end)
