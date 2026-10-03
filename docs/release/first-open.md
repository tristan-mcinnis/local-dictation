### First open (macOS will warn you)

Local Dictation is not notarized. It is a free project, and it has no paid Apple Developer ID. So macOS blocks the first open. Only open it if you downloaded it from the Releases page of this repository.

1. Open the DMG and drag Local Dictation to Applications.
2. Open Local Dictation once. macOS says it cannot verify the app. Click Done.
3. Open System Settings, then Privacy & Security. Scroll down and click Open Anyway next to Local Dictation. Confirm.

If you prefer Terminal, run this once, then open the app:

```bash
xattr -dr com.apple.quarantine "/Applications/Local Dictation.app"
```

Each release is signed ad hoc. So macOS may ask again for permissions such as Accessibility or Microphone after an update. Grant them again when asked.
