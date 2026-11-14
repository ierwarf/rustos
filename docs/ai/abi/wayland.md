# Wayland compositor extension status

`uiserver` owns Wayland protocol and clipboard policy in Ring 3. Linux DMA-BUF
client-buffer import (`zwp_linux_dmabuf_v1`) is specified in
`docs/ai/abi/app-dmabuf.md`, including its format limits and unverified items.

The display DVM's DMA-BUF path exports fixed compositor-owned atlas slots, not
client buffers. A client-buffer route needs an authenticated cross-domain
buffer capability, format/modifier discovery, producer synchronization, buffer
lifetime and revocation contracts, and integration with compositor rendering.
No advertised format is safe to assume until that design is implemented and
verified.

Surface viewport, buffer transform and scale state is captured at commit; viewport
changes use a compositor-owned copy retained past buffer release. Clipboard
selection is restricted to the focused client and recent input serials; stale
offers are invalidated on selection replacement or keyboard focus changes.
The selection event precedes keyboard enter for a newly focused client.
Surfaces default to client-side decorations. Decoration mode changes are
configured with the corresponding xdg-surface serial and take effect after
acknowledgement and commit.

Drag-and-drop tracks pointer grabs, offers, actions, drop and finish in the UI
service, including null-source drags and an icon surface. Every live recipient
data device receives enter, motion and leave, with separate negotiation state
per offer. The compositor selects exactly one eligible recipient at drop,
preferring v3 offers and then stable device-creation order. Only that recipient
receives drop; other offers are invalidated. Source feedback follows the current
eligible winner, so incompatible offers cannot erase another offer's action.
Version 3 offers require an accepted MIME type and a compatible selected action.
As specified by `wayland.xml`, version 1/2 accept is feedback rather than a
success gate; legacy targets use implicit copy when compatible. Only the winner
can resolve ask or finish. For an ask drop, the final source action is sent
immediately before successful completion, after any further transfer requests.
A legacy winner retains receive access until offer
destruction or client disconnect, which serves as its completion signal because
those versions have no finish request. Releasing a device does not retire its
already-dropped offer. Focus changes and source destruction invalidate affected
offers. A device whose offer cannot be allocated is not entered. Further
protocol and real-client testing remains necessary.

`xdg_toplevel.move` is supported for an authenticated pointer grab, but
client-requested interactive resize is not implemented. This limitation prevents
claiming complete client-side window-management support.
