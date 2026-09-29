-- Neovim 0.11+. Source this file or copy it to your nvim/plugin directory.
local binary = vim.g.scope_binary or "scope"
local group = vim.api.nvim_create_augroup("ScopeLsp", { clear = true })
local generation, previous = 0, nil

local function start(buf)
  if vim.bo[buf].buftype ~= "" then return end
  local root = vim.fs.root(buf, ".git")
    or vim.fs.root(buf, { "Cargo.toml", "go.mod" })
    or vim.fn.getcwd()
  root = vim.uv.fs_realpath(root) or root
  return vim.lsp.start({
    name = "scope",
    cmd = { binary, "lsp", root },
    root_dir = root,
  }, { bufnr = buf })
end

vim.api.nvim_create_autocmd("FileType", {
  group = group,
  pattern = { "rust", "go" },
  callback = function(args) start(args.buf) end,
})

local function search(query)
  local buf = vim.api.nvim_get_current_buf()
  local id = start(buf)
  local client = id and vim.lsp.get_client_by_id(id)

  if not client or not vim.wait(5000, function()
    return client.initialized or client:is_stopped()
  end, 50) or not client.initialized then
    vim.notify(
      "Scope did not initialize. Check :LspLog and the binary path.",
      vim.log.levels.ERROR
    )
    return
  end

  generation = generation + 1
  local current = generation
  if previous then previous.client:cancel_request(previous.id) end
  previous = nil

  local ok, request_id = client:request(
    "workspace/symbol", { query = query }, function(err, symbols)
      if current ~= generation then return end
      previous = nil

      if err then
        if err.code ~= -32800 then
          vim.notify("Scope: " .. err.message, vim.log.levels.ERROR)
        end
        return
      end

      if not symbols or #symbols == 0 then
        vim.notify("Scope: no matching symbols")
        return
      end

      vim.ui.select(symbols, {
        prompt = "Scope symbols (ranked)",
        format_item = function(symbol)
          
        local context = type(symbol.containerName) == "string" and symbol.containerName or nil
        local label = context and (context .. "::" .. symbol.name) or symbol.name
        label = label .. " [" .. (vim.lsp.protocol.SymbolKind[symbol.kind] or "Symbol") .. "]"
        local path = vim.fn.fnamemodify(
            vim.uri_to_fname(symbol.location.uri), ":~:."
          )
          return ("%s  %s:%d"):format(
            label, path, symbol.location.range.start.line + 1
          )
        end,
      }, function(symbol)
        if symbol and current == generation then
          vim.lsp.util.show_document(
            symbol.location, client.offset_encoding, { focus = true }
          )
        end
      end)
    end, buf
  )

  if ok then previous = { client = client, id = request_id } end
end

vim.api.nvim_create_user_command("ScopeSymbols", function(opts)
  if opts.args ~= "" then
    search(opts.args)
  else
    vim.ui.input({ prompt = "Scope query (kind:method path:src): " }, function(query)
      if query ~= nil then search(query) end
    end)
  end
end, {
  nargs = "*",
  desc = "Search live symbols using only Scope",
  force = true,
})

if vim.bo.filetype == "rust" or vim.bo.filetype == "go" then
  start(vim.api.nvim_get_current_buf())
end
