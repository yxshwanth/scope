# Searching with Scope

Use the same query in `scope find`, `scope browse`, and `:ScopeSymbols`.

CLI examples:

    scope find 'engtxn kind:method path:internal/engine'
    scope find 'Store open kind:method'
    scope find 'kind:fn,method path:"my files"'

In Neovim:

    :ScopeSymbols engtxn kind:method path:internal/engine

In the terminal browser, type the query without surrounding shell quotes.

Matching is case-insensitive. Every name term must match the name or its
enclosing context. Ranking prefers exact names, prefixes, substrings,
subsequences, limited typo matches, then context-only matches.
Queries of 5 to 32 characters tolerate one extra/mistyped query character.
This is a bounded heuristic, not a general spell checker.
Empty queries preserve file/location order.

Supported filters:

- kind:function or kind:fn
- kind:method
- kind:type
- kind:trait
- kind:fn,method means either kind
- Repeated kind filters are alternatives
- path:internal/engine matches a repository-relative path fragment
- Repeated path filters must all match
- path:"my files" supports spaces
- Path filters are substring filters, not globs

Rust context includes enclosing modules, types, trait implementations and
functions. Go methods include their receiver type. This is syntactic context
from Tree-sitter, not cross-file type resolution.

Run `scope index` once after upgrading an existing cache. Migration adds the
context column and invalidates old file hashes in the indexing transaction.
A failed migration rolls back. A successful migration reparses once.

CLI searches remain based on the saved index. The terminal browser follows
saved edits. Neovim overlays open buffers before ranking and filtering.

Restart Neovim after rebuilding to load the new server and picker labels.
Search currently ranks the indexed symbol set in memory.
