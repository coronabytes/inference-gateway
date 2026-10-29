# Inference Gateway

## Console UI (`ui/`)

Reuse the shared controls before writing a new one. `ui/src/components/controls.tsx` holds the form
controls, each a pill with its caption inside, before the value:

- `TextInput`: single-line text, with `mono` for names and URLs and `secret` for API keys and client secrets.
- `TextArea`: multi-line text, with the caption on the box's first line.
- `Select`: a dropdown (`SearchSelect`). It fills its column in a `.form` and fits its content in a `.filters` bar.
- `MultiSelect`: a dropdown that picks any number of options. `none` (default "All") clears the choice.
- `KeyValueInput`: `key=value` chips with suggested keys, then that key's values; typed pairs work too. For request labels use `LabelFilter` (filter bars) or `LabelEditor` (labels to send), in `components/LabelInput.tsx`.
- `Segmented`: one choice among a few (a tab-select). `children` go after the choices, for example a `Switch`.
- `Toggle`: an on/off `Switch` in a pill. Use it for checkboxes too.
- `FileButton`: opens the file chooser. With a `caption` it sits in a pill like the other controls.
- `Button`: with `variant` `primary` (the main action), `ghost` (quiet) or `danger`.
- `MenuButton`: a button (for a table row's commands, `className="icon-btn"` with `MenuIcon`) that opens a menu of actions such as Duplicate and Delete. Rows and cards show Edit as an icon first, then this menu.
- `PageActions`: a page's buttons at the end of its `.page-head`. When they don't fit beside the filters, the secondary ones fold into a "More" menu and the `primary` ones stay.
- `Field`: a label above content that is not a single control. It is rarely needed.

Every control takes `caption` (the text inside the pill, also its accessible name unless `label` is
given) and an optional `hint`. The hint is not shown inline. Clicking the caption opens it as a
popover under the control, and the caption turns link-coloured on hover to show that it can be
clicked. The hint is always the control's `aria-describedby`. `components/hint.tsx` (`HintBox`, `Caption`, `useHint`) provides this. Text
that must stay visible, such as validation errors, goes in `HintBox`'s `note` or a line below the
control. Pass `required` to a field the form cannot be saved without (to `FileButton` only while
no file is chosen); it has an amber border while empty. `TagEditor` and `TagPicker` (`components/Tags.tsx`) and `CatalogPicker` follow the same
pattern for tags and price catalog lookups.

Dialogs use `Modal` (`components/ui.tsx`) and pass their buttons as `footer`: the title and the
footer stay in view and only the content between them scrolls. A control's popup (a dropdown list,
suggestions, a hint) goes on its own layer with `Floating` and `useFloating`
(`components/floating.tsx`), so a dialog's scrolling body cannot clip it.

If none of these fits, add the new control to `controls.tsx` in the same style (`.control` and
`.control-caption` in `styles.css`) rather than styling a bare element in a page.
