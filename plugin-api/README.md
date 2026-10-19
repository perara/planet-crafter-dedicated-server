# pcds-plugin-api

The plugin API of the Planet Crafter dedicated server: the events a plugin receives, what it may
ask of the world and do to it, the byte format both sides share, the `Plugin` trait and the
`export!` macro that turns it into a WebAssembly module. `no_std` with `alloc`, no dependencies,
so it builds offline.

Licensed MIT or Apache-2.0 (see the two LICENSE files), so a plugin that includes it keeps its
own licence; the server itself is GPL-3.0-or-later. docs/plugins.md, "Licensing", says what that
means in practice. The guide is docs/plugins-guide.md, the reference docs/plugins-api.md.
