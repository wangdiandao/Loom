//! Shell-scoped dialogs shared across multiple pages.
//!
//! Feature-specific dialogs (such as `Details`'s `SetAffinityDialog` or wait-chain tree)
//! MUST live inside their own `features/*` slice and mount via `gpui-kit`'s `Root` dialog
//! layer. Only truly cross-page shell dialogs (such as `RunNewTaskDialog` arriving in
//! `0.0.3` / Issue #10) belong in this module.
