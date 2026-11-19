# Roadmap

## Diff

- Rich (rendered) diff: a segmented source / rendered toggle in the file
  header for files that have a rich view, markdown first ("Display the rich
  diff").
- Honor `.gitattributes` (`-diff`, `binary`) in binary detection; today it is
  git's NUL-in-8000-bytes rule only.

## Forges

- GitLab `edited_at` is always empty (REST has no last-edited time); fetch it
  through GraphQL if edit markers matter.
- GitLab offset suggestions (`suggestion:-N+M`) are not offered from the app.
- Forgejo thread reply and resolve need Forgejo v18 (17.0.1 has no endpoint).

## Tabs

- Pin a tab (the tab menu has "Keep open" but no pin).

## Terminals and machines

- Process rows: foreground or background, exit state, port chip, stop, and an
  expandable output tail.
- Host tree with connection state and Reconnect.

## Platform

- Launch to cached inbox is about 330 ms p95, about 200 ms of it gpui creating
  its DirectX device; revisit if gpui-pre moves that off the startup path.
- Audit that every clickable element shows a pointer, every resize handle a
  resize cursor, and text an I-beam (unverified: cursors are set widely, no
  full audit found).

## Agent daemon

Not planned yet.

- Per-machine daemon that owns PTYs so sessions outlive the app.
- Agent status from Claude Code hooks: four states (working, waiting on you,
  done, error), an "N waiting on you" bar, three-line session rows.
- Rules, such as "CI failed, start an agent".
- Service mode that watches subscriptions and notifies (webhook, Discord)
  while the app is closed.

## Open decisions

- CI shows twice in the Conversation: the CI box at the top and the merge
  box at the bottom. Recommend dropping the top box, as GitHub does.
- "Review with agent" refuses a checkout with symlinks that escape it.
  Recommend listing those links on the "Open on machine" config card too.
- The "from Claude" label on drafts is lost on restart. Recommend keeping it
  (a store column).
- Inbox "...": rows now have a "..." that opens the row menu. Should the repo
  group header's "..." (hide repository, open on forge) also become the full
  context menu?
