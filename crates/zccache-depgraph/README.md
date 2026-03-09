# zccache-depgraph

Dependency graph for include-aware cache invalidation.

Tracks `#include` relationships between source files and headers, resolves include paths against `-I`/`-isystem`/`-iquote`/`-idirafter` search dirs, and determines whether a compilation can use a cached artifact or needs recompilation.
