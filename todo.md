# Frontend Implementation: macOS Desktop Interactivity

To enable interactivity on macOS (since the window ignores mouse events to allow clicking desktop icons), the backend emits a `mac-desktop-click` event. The frontend must listen for this event and simulate a click on the corresponding HTML element.

## React Implementation Example

Add this effect to your main component (e.g., `App.tsx`):

```javascript
import { listen } from '@tauri-apps/api/event';
import { useEffect } from 'react';

export function useMacInteractivity() {
    useEffect(() => {
        // Listen for the custom mouse hook event from Rust
        const unlisten = listen('mac-desktop-click', (event) => {
            // event.payload contains [x, y] coordinates
            const [x, y] = event.payload as [number, number];

            // 1. Find the HTML element at these coordinates
            const element = document.elementFromPoint(x, y);

            // 2. If it's an interactive element, simulate a click
            if (element) {
                element.dispatchEvent(new MouseEvent('click', {
                    view: window,
                    bubbles: true,
                    cancelable: true,
                    clientX: x,
                    clientY: y
                }));
            }
        });

        return () => {
            unlisten.then(fn => fn());
        };
    }, []);
}
```

## Platform Note: Permissions

On macOS, the first time the application runs, it will trigger a system alert:
**"MyWallpaper Desktop" would like to control this computer using accessibility features.**

The user must authorize the app in:
`System Settings > Privacy & Security > Accessibility`

Without this permission, the `CGEventTap` will fail to attach, and interactivity will not work on macOS.
