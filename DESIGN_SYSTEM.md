# Design System
# 01 Brand Identity
Focused, creative, trustworthy, efficient, and approachable. The interface should feel like a quiet editing workspace: the media and captions receive the most attention, while AI suggestions remain easy to inspect and override.

# 02 Colour Palette
| Name | Hex | Use for |
|------------|---------|-----------------------------|
| Primary | #E9EEF5 | Main text and high-emphasis labels on dark surfaces |
| Accent | #FF7A45 | Primary actions, selected ranges, active states, and focus accents |
| Secondary | #7AA7FF | Links, secondary emphasis, and timeline markers |
| Background | #11151B | Main application canvas |
| Success | #42C99A | Completed jobs and successful publication |
| Error | #F46D75 | Errors and destructive actions |
| Muted | #9AA5B3 | Secondary text and inactive icons |

Use translucent Primary overlays and opacity variants of these palette colors for panels, borders, hover states, and timeline shading. Maintain readable contrast for captions and controls; use a Background-colored shadow or outline behind captions when the video is bright. Never rely on color alone to communicate status.

# 03 Typography
- **Font:** Inter for the interface; use a bundled, license-compatible sans serif fallback when Inter is unavailable. Caption presets may use Inter weights only in version 1.
- **H1:** 28px, Bold; project and primary screen titles.
- **H2:** 22px, Semibold; panel titles.
- **H3:** 18px, Semibold; section and clip titles.
- **Body:** 14px, Regular; controls, transcript, and descriptions.
- **Caption:** 12px, Medium; timestamps, metadata, and helper text. Video subtitles are sized relative to the export canvas, with a minimum readable size verified in preview.

# 04 Spacing
Use an 8px scale: 4, 8, 16, 24, 32, 48, 64px. Keep dense editing controls on the 4px and 8px increments, and use 16px or more between panels. Keyboard focus must remain visible at every density.

# 05 Border Radius
- Small elements (badges, inputs): 8px.
- Cards and panels: 12px.
- Buttons: 8px.
- Video preview and timeline tracks: 8px; do not round the actual exported frame.

# 06 Components
- **Primary button:** Accent background with dark text, 8px radius; reserved for actions such as Transcribe, Export, and Publish.
- **Secondary button:** Transparent or panel background with a 1px muted border; used for reversible actions.
- **Inputs:** Panel background, 1px muted border, 8px radius, accent focus ring, and persistent text labels for credentials and metadata.
- **Media player:** Large preview with play/pause, seek, frame or short-step controls, audio level, source time, and export-frame guides.
- **Transcript row:** Searchable spoken text with timestamps and speaker labels; highlight the current words and mark user corrections distinctly.
- **Suggestion card:** Hook, duration, source quote, editable rationale, and Accept or Reject actions. Selecting a card seeks the player to its proposed range.
- **Timeline range:** Clear in/out handles with keyboard adjustment and a text readout of exact times.
- **Job status:** Named stage, progress when measurable, elapsed time, retry action, and a plain-language error message.
- **Publish review:** Final video preview, destination account, caption and hashtags, privacy or visibility settings offered by the provider, and an explicit Publish button.
- **Credential settings:** A provider card shows only whether a key is saved, with a labeled save or replace input and an explicit confirmation before removal. Never display the stored secret.
- **Desktop window controls:** The Windows window is borderless. Keep a quiet, draggable strip in the main canvas with labeled minimize, maximize or restore, and close controls at the upper right. The product name remains in the fixed sidebar. The controls stay visible while workspace content scrolls and use the existing palette and focus style.

# 07 Icons
Use Lucide React. Line icons only, 1.5px stroke. Every icon-only control needs an accessible name and a tooltip. Do not use an icon as the sole indication of a destructive or publishing action.

# 08 Breakpoints
- Compact desktop window: under 900px wide; stack panels and keep the preview and primary action visible.
- Standard desktop window: 900px to 1439px wide; preview and transcript share the main area with a collapsible suggestions panel.
- Wide desktop window: 1440px and above; show preview, transcript, and suggestions together.

# Rules
- Never introduce a new UI color or font without updating this file.
- Design for desktop windows and keyboard use first. The app must remain usable in a compact window; phone support is outside version 1.
- Provide visible focus, sensible tab order, labels, and alternatives to drag-only interactions.
- Keep playback, editing, and export controls stable while background AI jobs run.
- Display provider cost or usage estimates as estimates, and keep publish and delete actions explicit.

# Public Website
The landing page extends the Cortelol palette and Inter typography outside the desktop application. Use Background `#11151B` as the main canvas, Primary `#E9EEF5` for copy, Accent `#FF7A45` for the main action and timeline, Secondary `#7AA7FF` for supporting links, and Muted `#9AA5B3` for explanatory details. Dark panels may use translucent Primary overlays as in the application. The marketing page can use a larger display type scale than the desktop UI while keeping body text readable and restrained.

The main visual pattern is a scroll-controlled sequence through the actual workflow: import, transcription, moment review, clip editing, and export. Its media and interface content are synthetic illustrations, not a recording of an actual user's project. The sequence should remain understandable by keyboard, on compact screens, and with reduced motion enabled. Language choice must be explicit and the selected language must apply to all public copy and controls. The installer call to action must reflect the actual release state.
