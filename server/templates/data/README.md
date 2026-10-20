# {id}

A data plugin for the Planet Crafter dedicated server: `rules.toml` is all there is.

```bash
pcds plugin check .       # checks it
pcds plugin test .        # runs scenarios/*.toml against it, with a made-up world
pcds plugin install .     # copies it to the server's run/plugins/
pcds plugin enable {id}   # shows what it asks for; you approve it
```

The reference for the rules file: docs/plugins-api.md ("Rules files").
