# Terminal Environment Inheritance Fix

## Problem

When Zed starts and restores the last workspace (via `"restore_on_startup": "last_workspace"`), the integrated terminal does not inherit the environment of the launching terminal. This means environment variables like `PATH`, `VIRTUAL_ENV`, `JAVA_HOME`, etc. are not available in the Zed terminal.

The environment was only captured when Zed was launched from the CLI with specific file paths, but not when Zed restored a workspace on startup.

## Solution

Capture the shell environment at Zed startup and pass it through the workspace restoration flow, ensuring terminals inherit the launching terminal's environment.

## Files Changed

### 1. `crates/zed/src/main.rs`

**Function:** `restore_or_create_workspace()`

**Change:** Capture the current environment at the start of the function:

```rust
// Capture the current environment so terminals inherit it
// when Zed is launched from a terminal
#[cfg(not(target_os = "windows"))]
let env = Some(std::env::vars().collect::<collections::HashMap<_, _>>());
#[cfg(target_os = "windows")]
let env = None; // Windows inherits environment automatically
```

**Call sites updated:**
- `restore_multiworkspace()` - now receives `env.clone()`
- `workspace::OpenOptions` for remote workspaces - now includes `env: env.clone()`
- `workspace::open_new()` - now includes `env: env.clone()` in OpenOptions

### 2. `crates/workspace/src/workspace.rs`

**Function:** `restore_multiworkspace()`

**Signature change:**
```rust
// Before
pub async fn restore_multiworkspace(
    multi_workspace: SerializedMultiWorkspace,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<WindowHandle<MultiWorkspace>>

// After
pub async fn restore_multiworkspace(
    multi_workspace: SerializedMultiWorkspace,
    app_state: Arc<AppState>,
    env: Option<collections::HashMap<String, String>>,  // NEW
    cx: &mut AsyncApp,
) -> anyhow::Result<WindowHandle<MultiWorkspace>>
```

**Internal calls updated:**
- `open_workspace_by_id()` - now receives `env.clone()`
- `Workspace::new_local()` - now receives `env.clone()` instead of `None`

---

**Function:** `open_workspace_by_id()`

**Signature change:**
```rust
// Before
pub fn open_workspace_by_id(
    workspace_id: WorkspaceId,
    app_state: Arc<AppState>,
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    cx: &mut App,
) -> Task<anyhow::Result<WindowHandle<MultiWorkspace>>>

// After
pub fn open_workspace_by_id(
    workspace_id: WorkspaceId,
    app_state: Arc<AppState>,
    requesting_window: Option<WindowHandle<MultiWorkspace>>,
    env: Option<collections::HashMap<String, String>>,  // NEW
    cx: &mut App,
) -> Task<anyhow::Result<WindowHandle<MultiWorkspace>>>
```

**Internal call updated:**
- `Project::local()` - now receives `env` instead of `None`

## Platform Behavior

| Platform | Behavior |
|----------|----------|
| Linux/macOS | Captures `std::env::vars()` and passes explicitly |
| Windows | Passes `None` - Windows inherits environment automatically |

## Testing

To verify the fix works:

1. Open a terminal with custom environment variables:
   ```bash
   export MY_VAR="hello"
   export PATH="/custom/path:$PATH"
   ```

2. Launch Zed from that terminal:
   ```bash
   ./target/release/zed
   ```

3. Let Zed restore the last workspace (or restart Zed)

4. Open a terminal in Zed (`ctrl+` `)`)

5. Check environment variables:
   ```bash
   echo $MY_VAR    # Should print "hello"
   echo $PATH      # Should include "/custom/path"
   ```

## Related Settings

This fix works with the following setting in your Zed config:

```json
{
  "restore_on_startup": "last_workspace"
}
```

## Notes

- The `OpenOptions` struct already had an `env` field - this change ensures it's populated during workspace restoration
- The `Project::local()` function already accepted an environment parameter - this change ensures it receives the captured environment
- No changes were needed to the terminal spawning code - it already uses the project environment correctly
