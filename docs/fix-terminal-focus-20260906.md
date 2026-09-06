# Terminal focus regression, 2026-09-06

The focus-safety gate introduced in `759e7ab` treated a terminal text-entry
role with non-settable AX attributes as a read-only field. Ghostty's focused
element reports `AXTextArea`, and recent dictations were held with
`NotEditable` instead of reaching its clipboard insertion path.

The classifier now permits clipboard paste for a known terminal's text-entry
surface even when AX cannot write its value. Positive non-input controls still
refuse injection, including in terminals. Non-terminal read-only fields and
unknown focus remain refused. Fresh focus resolution, PID ownership, cancellation
and the existing clipboard routing are unchanged. There is no editor allowlist.

## Verification

- The new terminal regression test failed before the fix and passed afterward.
- Independent runs: **323** featureless library tests and **348** full-feature
  library tests passed, plus the separate evaluation/verification tests.
- Independent static review found no blocking defect. Its stale safety-contract
  documentation finding was corrected in the source and both architecture and
  acceptance documents.
- Release build passed. The signed app was staged and verified before replacement.
- The user approved restart, including loss of the in-memory retry buffer.
  At deployment preflight the former daemon had already exited; no active
  recording was stopped by the swap.
- Installed executable SHA-256:
  `393cdce87cecff2bd626036da248a378ad4dae581ade2259aafec66d78c5fa31`.
- Existing designated signing requirement is unchanged. The new process reached
  its event-tap-ready marker with no Accessibility or worker startup error.
- The prior signed app is retained at
  `~/Library/Application Support/HousePerformance/rollback-followup-20260906/Local Dictation.app`.

## Live confirmation

The user confirmed the live paste works after deployment. A content-free check
of the post-restart log found **three successful dictations**, a Ghostty focus
marker, **zero no-field holds**, zero injection failures and zero worker errors.
The installed daemon remained running. No transcript text was copied into this
record. Other applications' end-to-end UI behavior was not manually exercised;
their denial rules remain covered by the test suite.

The terminal exception does not distinguish a shell prompt from a pager or other
terminal program. Choose the intended terminal input before dictating. The rule
still refuses positively identified buttons, menus, page bodies and static text.
