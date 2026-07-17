# Onyx Luxury V2 sidebar

## Decision

Implement the user-approved V2 sidebar preview while preserving the existing account grid and all right-pane behavior.

## P0 scope

- Redesign only the global left sidebar and its responsive presentation.
- Keep exactly the existing destinations: Codex/tool accounts, Usage, Settings, and API when the existing non-Windows shape exposes it.
- Replace account-specific sidebar copy with stable page subtitles such as `Accounts`.
- Add a compact brand header, consistent icon medallions, an onyx selected surface, a faint decorative monogram, and a one-line version/author footer.
- Reuse the existing logo, Lucide icons, colors, and installed dependencies.

## Explicit invariants

- Do not change the account cards, quota layout, account handlers, dialogs, Usage content, Settings content, backend, persisted state, auto-switch logic, authentication, or Tauri commands.
- Do not add account names, `accN`, active-account state, quota, auto-switch, CLI status, badges, counters, or new navigation destinations to the sidebar.
- Do not add a font or UI dependency.
- Keep the sidebar usable at the configured 820px minimum window width.

## Visual contract

- Approximately 220px expanded width, deep neutral onyx surface, subtle satin depth, fine hairlines, and restrained lavender.
- Brand header: existing AS logo in a 34px inset frame plus `AI Account Switcher`.
- Navigation rows: 60px height, 32px consistent icon medallions, title plus existing page subtitle.
- Selected row: subtle inset surface, cool lavender/silver inner border, soft top-edge sheen, narrow rounded lavender accent.
- Unselected rows: borderless, quiet hover surface.
- Empty lower area: faint existing monogram at very low opacity, decorative only.
- Footer: `v0.5.5 · Hoàng Phan` as one compact line, opening the existing author URL.

## Responsive behavior

- Expanded sidebar at normal desktop widths.
- At narrow widths, keep all three labels and destinations readable; reduce padding and medallion size before removing information.
- No horizontal overflow in the workspace at 820px.

## Acceptance checklist

- [x] Right pane is unchanged from baseline apart from the explicitly requested main-panel scrollbar.
- [x] Main-content scrollbar is a narrow translucent overlay-style thumb with no bright track or arrow buttons.
- [x] Sidebar contains no account-specific or new status text.
- [x] Codex, Usage, Settings retain their current handlers and selected states.
- [x] Non-Windows API entry remains supported.
- [x] Author link remains wired to the existing URL.
- [x] TypeScript and frontend production build pass.
- [x] Rust unit tests pass.
- [x] Windows release builds, installs, and installed/release SHA-256 match.
- [x] Default and 820px live sidebar views are inspected.
- [x] Timeline contains dated evidence.

## Changelog

- 2026-07-18: Created from the user-approved Onyx Luxury V2 preview.
- 2026-07-18: Implemented and verified the sidebar-only release; scoped scrollbar styling to the main panel and sidebar navigation after independent review.
