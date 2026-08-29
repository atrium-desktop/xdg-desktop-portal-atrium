# aegis-portal-prompter

`aegis-portal-prompter` is the one-shot, out-of-process user interface host
for interactive portal requests that do not require compositor-owned
resources. The portal backend starts one independently supervised process for
each FileChooser, Account confirmation, or Secret password request, writes one
explicitly versioned JSON request to standard input, and reads one versioned
JSON response from standard output.

The process owns optics (iris/lens) file browsing, yes/no confirmation, and
masked password input and never connects to the compositor IPC socket. Its
process boundary prevents a slow filesystem, toolkit fault, or dialog crash
from blocking the backend or compositor. The backend remains responsible for
the D-Bus Request lifecycle and terminates only that process when the caller
closes its request.

The dialogs follow the aegis design language (the palette is mirrored
locally — the portal build graph stays independent of the Aegis repository)
and honour the system light/dark preference. The optics Rust bindings come
from the tagged `ming2k/optics` release like any other dependency; joint
development against a sibling optics checkout mirrors the Aegis workflow:
`cp .cargo/optics-local.toml .cargo/config.toml`.

One known visual difference from other toolkits: iris cannot yet import an
exported `wayland:` parent handle through xdg-foreign-v2, so prompts map as
independent windows rather than transient-for-parent dialogs.

## Direct UI Verification & Prompter Commands

Run the prompter directly from the workspace root to test and inspect any UI prompt:

### FileChooser: Save File Mode

```bash
cargo run -p aegis-portal-prompter << 'EOF'
{
  "version": 6,
  "prompt": {
    "kind": "file_chooser",
    "request": {
      "mode": "save_file",
      "app_id": "org.mozilla.firefox",
      "title": "Save Web Page",
      "accept_label": "Save",
      "modal": true,
      "parent_window": null,
      "multiple": false,
      "current_folder": null,
      "current_name": "index.html",
      "current_file": null,
      "filters": [
        {
          "label": "Webpage, Complete",
          "rules": [{ "kind": "glob", "value": "*.html" }]
        },
        {
          "label": "All Files",
          "rules": [{ "kind": "glob", "value": "*" }]
        }
      ],
      "current_filter": null,
      "choices": [],
      "files": []
    }
  }
}
EOF
```

### FileChooser: Open File Mode

```bash
cargo run -p aegis-portal-prompter << 'EOF'
{
  "version": 6,
  "prompt": {
    "kind": "file_chooser",
    "request": {
      "mode": "open_file",
      "app_id": "org.gnome.TextEditor",
      "title": "Open File",
      "accept_label": "Open",
      "modal": true,
      "parent_window": null,
      "multiple": true,
      "current_folder": null,
      "current_name": null,
      "current_file": null,
      "filters": [
        {
          "label": "All Files",
          "rules": [{ "kind": "glob", "value": "*" }]
        }
      ],
      "current_filter": null,
      "choices": [],
      "files": []
    }
  }
}
EOF
```

### Confirmation Dialog

```bash
cargo run -p aegis-portal-prompter << 'EOF'
{
  "version": 6,
  "prompt": {
    "kind": "confirm",
    "request": {
      "title": "Camera Access",
      "body": "org.example.App is requesting access to your camera.",
      "accept_label": "Allow",
      "modal": true,
      "parent_window": null
    }
  }
}
EOF
```

### Secret / Password Dialog

```bash
cargo run -p aegis-portal-prompter << 'EOF'
{
  "version": 6,
  "prompt": {
    "kind": "secret",
    "request": {
      "title": "Unlock Vault",
      "reason": "dev.aegis.Test requires your master password."
    }
  }
}
EOF
```

### Application Chooser

```bash
cargo run -p aegis-portal-prompter << 'EOF'
{
  "version": 6,
  "prompt": {
    "kind": "choose_app",
    "request": {
      "app_id": "dev.aegis.Test",
      "title": "Open With",
      "content_type": "text/plain",
      "parent_window": null,
      "apps": [
        { "id": "org.gnome.TextEditor.desktop", "name": "Text Editor", "icon": null },
        { "id": "io.neovim.nvim.desktop", "name": "Neovim", "icon": null }
      ],
      "choices": [
        { "id": "remember", "label": "Remember this choice", "options": [], "selected": "false" }
      ]
    }
  }
}
EOF
```
