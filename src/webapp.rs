//! Embedded asset-browser WebApp (served as static HTML/CSS/JS).

pub(crate) const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>ADS Asset Browser</title>
  <link rel="stylesheet" href="/style.css">
</head>
<body>
  <div id="auth" class="auth hidden">
    <form id="authForm" class="auth-card">
      <div class="auth-mark">ADS</div>
      <h1>ADS Asset Browser</h1>
      <p class="auth-note">Production asset store &mdash; authorization required</p>
      <input id="tokenInput" type="password" autocomplete="current-password" placeholder="Bearer token" spellcheck="false">
      <button type="submit" class="solid-btn">Unlock</button>
    </form>
  </div>

  <div class="shell">
    <header class="slate">
      <div class="brand">
        <span class="brand-badge">ADS</span>
        <span class="brand-sub">Asset&nbsp;Browser</span>
      </div>
      <label class="slate-field">
        <span class="microlabel">Profile</span>
        <select id="profileSelect"></select>
      </label>
      <div class="slate-search">
        <input id="searchInput" type="search" placeholder="Search assets, categories, departments&hellip;" spellcheck="false">
      </div>
      <div class="slate-right">
        <span class="schema-chip">SCHEMA&nbsp;V8</span>
        <span id="connectionLed" class="led" title="Connection"></span>
        <button id="refreshButton" type="button" class="ghost-btn">Rescan</button>
        <button id="logoutButton" type="button" class="ghost-btn">Lock</button>
      </div>
    </header>

    <aside class="rail">
      <nav class="rail-block">
        <h2 class="microlabel">Category</h2>
        <ul id="categoryList" class="rail-list"></ul>
      </nav>
      <nav class="rail-block">
        <h2 class="microlabel">Department</h2>
        <ul id="departmentList" class="rail-list"></ul>
      </nav>
      <div class="rail-foot">
        <div id="status" class="status"></div>
      </div>
    </aside>

    <main class="stage">
      <div class="stage-bar">
        <span id="assetCount" class="count">0 ASSETS</span>
      </div>
      <div id="assetGrid" class="grid"></div>
      <div id="gridEmpty" class="void hidden">
        <div class="void-cube"></div>
        <p>No assets match the current filters.</p>
      </div>
    </main>

    <aside class="inspector">
      <div id="detailEmpty" class="void">
        <div class="void-cube"></div>
        <p>Select an asset to inspect</p>
      </div>
      <div id="detailPanel" class="inspector-body hidden">
        <div class="inspector-head">
          <div class="inspector-title">
            <h2 id="detailTitle"></h2>
            <p id="detailSubtitle" class="mono"></p>
          </div>
          <span id="detailDepartment" class="dept-tag"></span>
        </div>
        <div id="detailPreview" class="preview"></div>

        <section class="inspector-section">
          <h3 class="microlabel">ADS URI</h3>
          <div class="uri-row">
            <input id="assetUriInput" type="text" readonly spellcheck="false">
            <button id="copyAssetUriButton" type="button" class="amber-btn">Copy</button>
          </div>
        </section>

        <section class="inspector-section">
          <h3 class="microlabel">Version</h3>
          <div class="version-controls">
            <select id="versionSelect"></select>
            <label class="force-check"><input id="forcePull" type="checkbox"><span>Force</span></label>
          </div>
          <div class="action-row">
            <button id="setCurrentButton" type="button" class="amber-btn">Pin Current</button>
            <button id="resetCurrentButton" type="button" class="ghost-btn">Reset</button>
          </div>
          <button id="pullButton" type="button" class="solid-btn block-btn">Pull to Workspace</button>
        </section>

        <section class="inspector-section">
          <div class="section-head">
            <h3 class="microlabel">WIP Stream</h3>
            <span id="wipSummary" class="manifest-summary"></span>
          </div>
          <div id="wipList" class="take-log wip-log"></div>
        </section>

        <section class="inspector-section">
          <h3 class="microlabel">Take Log</h3>
          <div id="versionList" class="take-log"></div>
        </section>

        <section class="inspector-section">
          <div class="section-head">
            <h3 class="microlabel">Manifest</h3>
            <span id="manifestSummary" class="manifest-summary"></span>
          </div>
          <div id="manifestList" class="manifest"></div>
        </section>

        <section class="inspector-section">
          <h3 class="microlabel">Thumbnail</h3>
          <div class="action-row">
            <label class="ghost-btn upload-btn">Upload<input id="thumbnailInput" type="file" accept="image/png,image/jpeg,image/webp"></label>
            <button id="copyThumbUrlButton" type="button" class="ghost-btn">Copy URL</button>
          </div>
        </section>
      </div>
    </aside>
  </div>
  <script src="/app.js"></script>
</body>
</html>
"#;

pub(crate) const STYLE_CSS: &str = r#":root {
  color-scheme: dark;
  --bg: #131210;
  --panel: #1a1816;
  --panel-2: #201d1a;
  --panel-3: #272320;
  --line: #2c2823;
  --line-strong: #3d372f;
  --ink: #e9e4da;
  --ink-dim: #9b948a;
  --ink-faint: #6e6759;
  --amber: #f2a93c;
  --amber-bright: #ffc14d;
  --amber-dim: rgba(242, 169, 60, .13);
  --green: #8cd97c;
  --red: #e0604a;
  --mono: 'Cascadia Code', Consolas, 'SF Mono', 'JetBrains Mono', monospace;
  --sans: Bahnschrift, 'Avenir Next Condensed', 'Segoe UI', 'Helvetica Neue', sans-serif;
  --radius: 4px;
}

* { box-sizing: border-box; }

html, body { height: 100%; }

body {
  margin: 0;
  font-family: var(--sans);
  font-size: 14px;
  color: var(--ink);
  background:
    radial-gradient(1100px 700px at 75% -12%, rgba(242, 169, 60, .045), transparent 60%),
    repeating-linear-gradient(0deg, rgba(255, 255, 255, .012) 0 1px, transparent 1px 3px),
    var(--bg);
  overflow: hidden;
}

::selection { background: var(--amber-dim); color: var(--amber-bright); }

:focus-visible { outline: 1px solid var(--amber); outline-offset: 1px; }

::-webkit-scrollbar { width: 10px; height: 10px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb { background: var(--line-strong); border-radius: 5px; border: 2px solid var(--bg); }
::-webkit-scrollbar-thumb:hover { background: var(--ink-faint); }

.hidden { display: none !important; }

.mono { font-family: var(--mono); }

.microlabel {
  margin: 0;
  font-size: 10px;
  font-weight: 600;
  letter-spacing: .16em;
  text-transform: uppercase;
  color: var(--ink-faint);
}

/* ---------- shell layout ---------- */

.shell {
  display: grid;
  grid-template-columns: 218px 1fr 348px;
  grid-template-rows: 52px 1fr;
  grid-template-areas:
    'slate slate slate'
    'rail  stage inspector';
  height: 100vh;
}

@media (max-width: 1280px) {
  .shell { grid-template-columns: 196px 1fr 312px; }
}

/* ---------- top slate ---------- */

.slate {
  grid-area: slate;
  display: flex;
  align-items: center;
  gap: 18px;
  padding: 0 16px;
  background: linear-gradient(180deg, var(--panel-2), var(--panel));
  border-bottom: 1px solid var(--line-strong);
}

.brand { display: flex; align-items: baseline; gap: 10px; }

.brand-badge {
  display: inline-block;
  padding: 3px 9px 2px;
  background: var(--amber);
  color: #181307;
  font-weight: 700;
  font-size: 15px;
  letter-spacing: .22em;
  border-radius: 2px;
  transform: translateY(-1px);
}

.brand-sub {
  font-size: 11px;
  letter-spacing: .24em;
  text-transform: uppercase;
  color: var(--ink-dim);
}

.slate-field { display: flex; align-items: center; gap: 8px; }

.slate-field select {
  min-width: 130px;
}

.slate-search { flex: 1; max-width: 520px; }

.slate-search input {
  width: 100%;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  padding: 7px 12px;
  color: var(--ink);
  font-family: var(--mono);
  font-size: 12.5px;
  transition: border-color .15s ease;
}

.slate-search input:hover { border-color: var(--line-strong); }
.slate-search input:focus { border-color: var(--amber); outline: none; }

.slate-right { display: flex; align-items: center; gap: 10px; margin-left: auto; }

.schema-chip {
  font-family: var(--mono);
  font-size: 10px;
  letter-spacing: .1em;
  color: var(--ink-faint);
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 3px 9px;
}

.led {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: var(--ink-faint);
  box-shadow: 0 0 0 0 transparent;
  transition: background .2s ease;
}

.led.on {
  background: var(--green);
  animation: pulse 2.4s ease-in-out infinite;
}

.led.err { background: var(--red); animation: none; }

@keyframes pulse {
  0%, 100% { box-shadow: 0 0 0 0 rgba(140, 217, 124, .35); }
  50% { box-shadow: 0 0 7px 2px rgba(140, 217, 124, .25); }
}

/* ---------- controls ---------- */

select {
  appearance: none;
  background: var(--panel-3);
  border: 1px solid var(--line-strong);
  border-radius: var(--radius);
  color: var(--ink);
  font-family: var(--mono);
  font-size: 12.5px;
  padding: 6px 26px 6px 10px;
  background-image: linear-gradient(45deg, transparent 50%, var(--ink-dim) 50%),
    linear-gradient(135deg, var(--ink-dim) 50%, transparent 50%);
  background-position: calc(100% - 15px) 55%, calc(100% - 10px) 55%;
  background-size: 5px 5px;
  background-repeat: no-repeat;
  cursor: pointer;
  transition: border-color .15s ease;
}

select:hover { border-color: var(--ink-faint); }
select:focus { border-color: var(--amber); outline: none; }

button { font-family: var(--sans); cursor: pointer; }

.ghost-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  background: transparent;
  border: 1px solid var(--line-strong);
  border-radius: var(--radius);
  color: var(--ink-dim);
  font-size: 12px;
  letter-spacing: .06em;
  padding: 6px 12px;
  transition: color .15s ease, border-color .15s ease, background .15s ease;
}

.ghost-btn:hover { color: var(--ink); border-color: var(--ink-faint); background: var(--panel-2); }

.amber-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  background: transparent;
  border: 1px solid var(--amber);
  border-radius: var(--radius);
  color: var(--amber);
  font-size: 12px;
  font-weight: 600;
  letter-spacing: .06em;
  padding: 6px 12px;
  transition: background .15s ease, color .15s ease;
}

.amber-btn:hover { background: var(--amber-dim); color: var(--amber-bright); }

.solid-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  background: var(--amber);
  border: 1px solid var(--amber);
  border-radius: var(--radius);
  color: #181307;
  font-size: 12.5px;
  font-weight: 700;
  letter-spacing: .08em;
  text-transform: uppercase;
  padding: 8px 14px;
  transition: background .15s ease, transform .1s ease;
}

.solid-btn:hover { background: var(--amber-bright); }
.solid-btn:active { transform: translateY(1px); }

.block-btn { width: 100%; margin-top: 8px; }

/* ---------- left rail ---------- */

.rail {
  grid-area: rail;
  display: flex;
  flex-direction: column;
  gap: 18px;
  padding: 16px 12px 12px;
  background: var(--panel);
  border-right: 1px solid var(--line);
  overflow-y: auto;
}

.rail-block .microlabel { padding: 0 6px 8px; }

.rail-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 1px; }

.rail-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 6px 9px;
  border-radius: 3px;
  border-left: 2px solid transparent;
  color: var(--ink-dim);
  font-size: 13px;
  cursor: pointer;
  transition: background .12s ease, color .12s ease;
}

.rail-item:hover { background: var(--panel-2); color: var(--ink); }

.rail-item.active {
  background: var(--amber-dim);
  border-left-color: var(--amber);
  color: var(--amber-bright);
}

.rail-label {
  display: flex;
  align-items: center;
  gap: 7px;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.dept-dot {
  flex-shrink: 0;
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: hsl(var(--dh) 50% 58%);
  box-shadow: 0 0 4px hsl(var(--dh) 50% 58% / .4);
}

.rail-item .n {
  font-family: var(--mono);
  font-size: 10.5px;
  color: var(--ink-faint);
}

.rail-item.active .n { color: var(--amber); }

.rail-foot { margin-top: auto; padding: 6px; }

.status {
  min-height: 16px;
  font-family: var(--mono);
  font-size: 11px;
  line-height: 1.5;
  color: var(--ink-faint);
  word-break: break-word;
}

.status.ok { color: var(--green); }
.status.err { color: var(--red); }

/* ---------- stage / asset grid ---------- */

.stage {
  grid-area: stage;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.stage-bar {
  display: flex;
  align-items: center;
  padding: 12px 18px 4px;
}

.count {
  font-family: var(--mono);
  font-size: 11px;
  letter-spacing: .14em;
  color: var(--ink-faint);
}

.grid {
  flex: 1;
  overflow-y: auto;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(168px, 1fr));
  /* Explicit row sizing: Chromium collapses `auto` rows to near-zero in
     scrollable grids with thousands of rows. */
  grid-auto-rows: max-content;
  gap: 12px;
  align-content: start;
  padding: 12px 18px 24px;
}

.card {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  overflow: hidden;
  cursor: pointer;
  animation: rise .4s ease backwards;
  transition: transform .15s ease, border-color .15s ease, box-shadow .15s ease;
}

.card:hover {
  transform: translateY(-2px);
  border-color: var(--line-strong);
  box-shadow: 0 8px 20px rgba(0, 0, 0, .45);
}

.card.active {
  border-color: var(--amber);
  box-shadow: 0 0 0 1px var(--amber), 0 10px 24px rgba(0, 0, 0, .5);
}

.card:nth-child(1) { animation-delay: .02s; }
.card:nth-child(2) { animation-delay: .05s; }
.card:nth-child(3) { animation-delay: .08s; }
.card:nth-child(4) { animation-delay: .11s; }
.card:nth-child(5) { animation-delay: .14s; }
.card:nth-child(6) { animation-delay: .17s; }
.card:nth-child(7) { animation-delay: .2s; }
.card:nth-child(8) { animation-delay: .23s; }
.card:nth-child(9) { animation-delay: .26s; }
.card:nth-child(10) { animation-delay: .29s; }
.card:nth-child(11) { animation-delay: .32s; }
.card:nth-child(12) { animation-delay: .35s; }

@keyframes rise {
  from { opacity: 0; transform: translateY(8px); }
  to { opacity: 1; transform: none; }
}

.thumb {
  position: relative;
  aspect-ratio: 4 / 3;
  background:
    url('data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 80 80%22%3E%3Cg fill=%22none%22 stroke=%22%23332e28%22 stroke-width=%221%22%3E%3Cpath d=%22M40 14 66 28v24L40 66 14 52V28z%22/%3E%3Cpath d=%22M40 14v24m0 0L14 28m26 10 26-10M40 38v28%22/%3E%3C/g%3E%3C/svg%3E') center / 64px no-repeat,
    linear-gradient(160deg, var(--panel-2), var(--panel));
  border-bottom: 1px solid var(--line);
}

.thumb img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
}

.dept-badge {
  position: absolute;
  top: 6px;
  left: 6px;
  max-width: calc(100% - 12px);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: var(--mono);
  font-size: 9px;
  font-weight: 600;
  letter-spacing: .1em;
  text-transform: uppercase;
  padding: 2px 7px;
  border-radius: 2px;
  background: rgba(12, 11, 9, .78);
  border: 1px solid hsl(var(--dh) 45% 60% / .55);
  color: hsl(var(--dh) 55% 70%);
  pointer-events: none;
}

.card-meta { padding: 9px 10px 10px; display: flex; flex-direction: column; gap: 4px; }

.card-name {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
  font-weight: 600;
  font-size: 13.5px;
  letter-spacing: .02em;
}

.card-name .ver {
  font-family: var(--mono);
  font-size: 11px;
  font-weight: 400;
  color: var(--amber);
}

.card-sub {
  font-size: 11px;
  color: var(--ink-faint);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ---------- empty states ---------- */

.void {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  color: var(--ink-faint);
  font-size: 12.5px;
  letter-spacing: .04em;
  padding: 32px;
  text-align: center;
}

.void-cube {
  width: 72px;
  height: 72px;
  background: url('data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 80 80%22%3E%3Cg fill=%22none%22 stroke=%22%233d372f%22 stroke-width=%221.2%22%3E%3Cpath d=%22M40 10 70 26v28L40 70 10 54V26z%22/%3E%3Cpath d=%22M40 10v28m0 0L10 26m30 12 30-12M40 38v32%22/%3E%3C/g%3E%3C/svg%3E') center / contain no-repeat;
  opacity: .9;
}

/* ---------- inspector ---------- */

.inspector {
  grid-area: inspector;
  display: flex;
  flex-direction: column;
  background: var(--panel);
  border-left: 1px solid var(--line);
  overflow-y: auto;
}

.inspector-body {
  display: flex;
  flex-direction: column;
  gap: 18px;
  padding: 18px 16px 24px;
  animation: rise .3s ease backwards;
}

.inspector-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 10px;
}

.inspector-title h2 {
  margin: 0;
  font-size: 19px;
  font-weight: 700;
  letter-spacing: .02em;
}

.inspector-title p {
  margin: 3px 0 0;
  font-size: 11px;
  color: var(--ink-faint);
}

.dept-tag {
  flex-shrink: 0;
  font-family: var(--mono);
  font-size: 10.5px;
  letter-spacing: .08em;
  text-transform: uppercase;
  color: hsl(var(--dh, 38) 55% 70%);
  background: hsl(var(--dh, 38) 45% 60% / .1);
  border: 1px solid hsl(var(--dh, 38) 45% 60% / .5);
  border-radius: 2px;
  padding: 3px 8px;
}

.preview {
  position: relative;
  aspect-ratio: 16 / 10;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  overflow: hidden;
  background:
    url('data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 80 80%22%3E%3Cg fill=%22none%22 stroke=%22%23332e28%22 stroke-width=%221%22%3E%3Cpath d=%22M40 14 66 28v24L40 66 14 52V28z%22/%3E%3Cpath d=%22M40 14v24m0 0L14 28m26 10 26-10M40 38v28%22/%3E%3C/g%3E%3C/svg%3E') center / 72px no-repeat,
    linear-gradient(160deg, var(--panel-2), var(--bg));
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--ink-faint);
  font-size: 11.5px;
  letter-spacing: .06em;
}

.preview img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
}

.inspector-section { display: flex; flex-direction: column; gap: 9px; }

.uri-row { display: flex; gap: 8px; }

.uri-row input {
  flex: 1;
  min-width: 0;
  background: var(--bg);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  color: var(--amber-bright);
  font-family: var(--mono);
  font-size: 11.5px;
  padding: 7px 10px;
}

.uri-row input:focus { border-color: var(--amber); outline: none; }

.version-controls { display: flex; align-items: center; gap: 10px; }

.version-controls select { flex: 1; }

.force-check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--ink-dim);
  cursor: pointer;
  user-select: none;
}

.force-check input { accent-color: var(--amber); }

.action-row { display: flex; gap: 8px; }

.action-row > * { flex: 1; }

.upload-btn { position: relative; overflow: hidden; }

.upload-btn input { position: absolute; inset: 0; opacity: 0; cursor: pointer; }

/* ---------- take log ---------- */

.take-log {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  overflow: hidden;
  max-height: 300px;
  overflow-y: auto;
}

.take {
  display: grid;
  grid-template-columns: auto 1fr auto;
  align-items: center;
  gap: 10px;
  padding: 7px 10px;
  border-left: 2px solid transparent;
  border-bottom: 1px solid var(--line);
  background: var(--panel);
  cursor: pointer;
  transition: background .12s ease;
}

.take:last-child { border-bottom: none; }

.take:hover { background: var(--panel-2); }

.take.selected { background: var(--panel-3); }

.take.current { border-left-color: var(--amber); background: var(--amber-dim); }

.take .v {
  font-family: var(--mono);
  font-size: 12px;
  font-weight: 600;
  color: var(--ink);
}

.take.current .v { color: var(--amber-bright); }

.take .meta {
  font-family: var(--mono);
  font-size: 10.5px;
  color: var(--ink-faint);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.take .tag {
  font-family: var(--mono);
  font-size: 9px;
  font-weight: 700;
  letter-spacing: .12em;
  padding: 2px 6px;
  border-radius: 2px;
}

.take .tag.pin { background: var(--amber); color: #181307; }

.take .tag.latest { border: 1px solid var(--line-strong); color: var(--ink-faint); }

/* ---------- wip stream ---------- */

.take .tag.head { border: 1px solid var(--amber); color: var(--amber); }

.wip-actions {
  display: flex;
  align-items: center;
  gap: 6px;
}

.promote-btn {
  font-family: var(--mono);
  font-size: 9px;
  font-weight: 700;
  letter-spacing: .12em;
  padding: 2px 8px;
  background: transparent;
  border: 1px solid var(--line-strong);
  border-radius: 2px;
  color: var(--ink-dim);
  transition: color .15s ease, border-color .15s ease, background .15s ease;
}

.promote-btn:hover { border-color: var(--amber); color: var(--amber-bright); background: var(--amber-dim); }

/* ---------- manifest ---------- */

.section-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
}

.manifest-summary {
  font-family: var(--mono);
  font-size: 10px;
  letter-spacing: .06em;
  color: var(--ink-faint);
}

.manifest {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  max-height: 260px;
  overflow-y: auto;
}

.file-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 10px;
  border-bottom: 1px solid var(--line);
  background: var(--panel);
  cursor: pointer;
  transition: background .12s ease;
}

.file-row:last-child { border-bottom: none; }

.file-row:hover { background: var(--panel-2); }

.file-row:hover .file-path { color: var(--amber-bright); }

.file-dot {
  flex-shrink: 0;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: hsl(var(--dh) 50% 58%);
}

.file-dot.neutral { background: var(--line-strong); }

.file-path {
  flex: 1;
  min-width: 0;
  font-family: var(--mono);
  font-size: 11px;
  color: var(--ink-dim);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  transition: color .12s ease;
}

.file-size {
  flex-shrink: 0;
  font-family: var(--mono);
  font-size: 10px;
  color: var(--ink-faint);
}

/* ---------- auth gate ---------- */

.auth {
  position: fixed;
  inset: 0;
  z-index: 50;
  display: flex;
  align-items: center;
  justify-content: center;
  background:
    radial-gradient(900px 600px at 50% 0%, rgba(242, 169, 60, .05), transparent 60%),
    rgba(12, 11, 10, .92);
  backdrop-filter: blur(6px);
}

.auth-card {
  display: flex;
  flex-direction: column;
  gap: 14px;
  width: min(360px, calc(100vw - 48px));
  padding: 30px 28px 28px;
  background: var(--panel);
  border: 1px solid var(--line-strong);
  border-top: 2px solid var(--amber);
  border-radius: 6px;
  box-shadow: 0 24px 60px rgba(0, 0, 0, .6);
  animation: rise .35s ease backwards;
}

.auth-mark {
  align-self: flex-start;
  padding: 4px 12px 3px;
  background: var(--amber);
  color: #181307;
  font-weight: 700;
  font-size: 18px;
  letter-spacing: .26em;
  border-radius: 2px;
}

.auth-card h1 { margin: 2px 0 0; font-size: 17px; font-weight: 600; letter-spacing: .03em; }

.auth-note {
  margin: 0;
  font-family: var(--mono);
  font-size: 10px;
  letter-spacing: .12em;
  text-transform: uppercase;
  color: var(--ink-faint);
}

.auth-card input {
  background: var(--bg);
  border: 1px solid var(--line-strong);
  border-radius: var(--radius);
  color: var(--ink);
  font-family: var(--mono);
  font-size: 13px;
  padding: 9px 12px;
}

.auth-card input:focus { border-color: var(--amber); outline: none; }
"#;

pub(crate) const APP_JS: &str = r#"const state = {
  token: sessionStorage.getItem('adsToken') || '',
  profile: '',
  allAssets: [],
  assets: [],
  category: '',
  department: '',
  selected: null,
  versions: [],
  wips: [],
  selectedVersion: '',
  manifestCache: new Map()
};

const $ = (id) => document.getElementById(id);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
// Versions are plain integers on the wire (schema v8); v### is display-only sugar.
const fmtVersion = (version) => (version === null || version === undefined || version === '') ? '-' : 'v' + String(version).padStart(3, '0');

// Department badge hues: fixed palette for the canonical departments, stable
// hash fallback for custom names. The amber zone (~38) is reserved for the
// app accent, so fx sits at red instead.
const DEPT_HUES = {model: 210, lookdev: 268, texture: 175, textures: 175, tex: 175, rig: 330, anim: 110, layout: 75, fx: 8};
function deptHue(name) {
  const key = String(name || '').toLowerCase();
  if (key in DEPT_HUES) return DEPT_HUES[key];
  let hash = 0;
  for (const ch of key) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
  return hash % 360;
}

let statusTimer = null;
function status(text, kind) {
  const node = $('status');
  node.textContent = text || '';
  node.className = 'status' + (kind ? ' ' + kind : '');
  clearTimeout(statusTimer);
  if (text && kind === 'ok') {
    statusTimer = setTimeout(() => { node.textContent = ''; node.className = 'status'; }, 4000);
  }
}

function led(stateName) {
  $('connectionLed').className = 'led' + (stateName ? ' ' + stateName : '');
}

function humanBytes(bytes) {
  if (!Number.isFinite(bytes)) return '-';
  if (bytes < 1024) return bytes + ' B';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = '';
  for (const next of units) {
    value /= 1024;
    unit = next;
    if (value < 1024) break;
  }
  return value.toFixed(value < 10 ? 1 : 0) + ' ' + unit;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (_) {
    const area = document.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.left = '-9999px';
    document.body.appendChild(area);
    area.focus();
    area.select();
    area.setSelectionRange(0, area.value.length);
    const ok = document.execCommand('copy');
    document.body.removeChild(area);
    return ok;
  }
}

async function api(path, options = {}) {
  const headers = options.headers ? new Headers(options.headers) : new Headers();
  headers.set('Authorization', `Bearer ${state.token}`);
  if (options.body && !(options.body instanceof FormData)) headers.set('Content-Type', 'application/json');
  const res = await fetch(path, {...options, headers});
  if (res.status === 401) {
    sessionStorage.removeItem('adsToken');
    led('err');
    $('auth').classList.remove('hidden');
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try { message = (await res.json()).error || message; } catch (_) {}
    throw new Error(message);
  }
  return res.json();
}

async function apiBlob(path, mimeType = '') {
  const headers = new Headers();
  headers.set('Authorization', `Bearer ${state.token}`);
  const res = await fetch(path, {headers});
  if (res.status === 401) {
    sessionStorage.removeItem('adsToken');
    led('err');
    $('auth').classList.remove('hidden');
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try { message = (await res.json()).error || message; } catch (_) {}
    throw new Error(message);
  }
  const blob = await res.blob();
  return mimeType ? blob.slice(0, blob.size, mimeType) : blob;
}

function qs(params) {
  const search = new URLSearchParams();
  Object.entries(params).forEach(([key, value]) => {
    if (value !== undefined && value !== null && value !== '') search.set(key, value);
  });
  return search.toString();
}

async function init() {
  if (!state.token) {
    $('auth').classList.remove('hidden');
    led('');
    return;
  }
  $('auth').classList.add('hidden');
  const data = await api('/api/profiles');
  const select = $('profileSelect');
  select.innerHTML = data.profiles.map(p => `<option value="${esc(p.name)}">${esc(p.name)}</option>`).join('');
  state.profile = select.value || data.profiles[0]?.name || '';
  led('on');
  await loadAssets();
}

async function loadAssets() {
  if (!state.profile) return;
  status('Scanning store…');
  const params = {profile: state.profile, q: $('searchInput').value};
  const data = await api(`/api/assets?${qs(params)}`);
  state.allAssets = data.assets;
  status('');
  applyFilters();
}

function applyFilters() {
  if (state.category && !state.allAssets.some(a => a.category === state.category)) state.category = '';
  if (state.department && !state.allAssets.some(a => a.department === state.department)) state.department = '';
  state.assets = state.allAssets.filter(a =>
    (!state.category || a.category === state.category) &&
    (!state.department || a.department === state.department));
  renderRails();
  renderGrid();
}

function railItems(values, selected, withDot) {
  const counts = new Map();
  values.forEach(v => counts.set(v, (counts.get(v) || 0) + 1));
  const keys = [...counts.keys()].sort();
  const label = (key) => withDot
    ? `<span class="rail-label"><span class="dept-dot" style="--dh:${deptHue(key)}"></span>${esc(key)}</span>`
    : `<span class="rail-label">${esc(key)}</span>`;
  const all = `<li class="rail-item${selected === '' ? ' active' : ''}" data-value=""><span class="rail-label">All</span><span class="n">${values.length}</span></li>`;
  return all + keys.map(key =>
    `<li class="rail-item${selected === key ? ' active' : ''}" data-value="${esc(key)}">${label(key)}<span class="n">${counts.get(key)}</span></li>`
  ).join('');
}

function renderRails() {
  $('categoryList').innerHTML = railItems(state.allAssets.map(a => a.category), state.category, false);
  $('departmentList').innerHTML = railItems(state.allAssets.map(a => a.department), state.department, true);
  $('categoryList').querySelectorAll('.rail-item').forEach(item => {
    item.addEventListener('click', () => { state.category = item.dataset.value; applyFilters(); });
  });
  $('departmentList').querySelectorAll('.rail-item').forEach(item => {
    item.addEventListener('click', () => { state.department = item.dataset.value; applyFilters(); });
  });
}

function renderGrid() {
  const count = state.assets.length;
  $('assetCount').textContent = `${count} ASSET${count === 1 ? '' : 'S'}`;
  $('gridEmpty').classList.toggle('hidden', count > 0);
  $('assetGrid').innerHTML = state.assets.map(asset => {
    const key = assetKey(asset);
    const active = state.selected && assetKey(state.selected) === key ? ' active' : '';
    const hasThumb = asset.thumbnail_url || asset.thumbnail_sha256;
    const thumb = hasThumb ? `<img data-thumb-key="${esc(key)}" alt="" loading="lazy" onerror="this.remove()">` : '';
    return `<article class="card${active}" data-key="${esc(key)}">
      <div class="thumb">${thumb}<span class="dept-badge" style="--dh:${deptHue(asset.department)}">${esc(asset.department)}</span></div>
      <div class="card-meta">
        <div class="card-name"><span>${esc(asset.asset_code)}</span><span class="ver">${fmtVersion(asset.current)}</span></div>
        <div class="card-sub">${esc(asset.category)}</div>
        <div class="card-sub">${asset.version_count} versions · latest ${fmtVersion(asset.latest)}</div>
      </div>
    </article>`;
  }).join('');
  document.querySelectorAll('.card').forEach(card => {
    card.addEventListener('click', () => {
      const asset = state.assets.find(item => assetKey(item) === card.dataset.key);
      if (asset) selectAsset(asset).catch(showError);
    });
  });
  hydrateGridThumbnails();
}

function setActiveCard(key) {
  document.querySelectorAll('.card.active').forEach(card => card.classList.remove('active'));
  const card = document.querySelector(`.card[data-key="${CSS.escape(key)}"]`);
  if (card) card.classList.add('active');
}

async function selectAsset(asset) {
  state.selected = asset;
  state.selectedVersion = asset.current || asset.latest || '';
  // Class swap instead of re-rendering the grid: with thousands of cards a
  // full rebuild costs ~80ms per click and reloads thumbnails.
  setActiveCard(assetKey(asset));
  const params = {profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department};
  const [data, wipData] = await Promise.all([
    api(`/api/versions?${qs(params)}`),
    api(`/api/wips?${qs(params)}`)
  ]);
  state.versions = data.versions;
  state.thumbnails = data.thumbnails;
  state.wips = wipData.wips;
  state.lastCurrentStatus = data.current_status;
  renderDetail(asset, data);
  renderWips();
  await loadManifest($('versionSelect').value);
}

function renderDetail(asset, data) {
  $('detailEmpty').classList.add('hidden');
  $('detailPanel').classList.remove('hidden');
  $('detailTitle').textContent = asset.asset_code;
  $('detailSubtitle').textContent = asset.category;
  $('detailDepartment').textContent = asset.department;
  $('detailDepartment').style.setProperty('--dh', deptHue(asset.department));
  const options = data.versions.map(v => `<option value="${esc(v.version)}">${fmtVersion(v.version)}</option>`).join('');
  $('versionSelect').innerHTML = options;
  $('versionSelect').value = String(state.selectedVersion || data.current_status.current || data.current_status.latest || '');
  renderDetailThumbnail(asset, data.thumbnails || []);
  updateAssetUriField();
  renderTakeLog(data);
}

const thumbObjectUrls = new Map();

async function thumbnailObjectUrl(asset) {
  if (asset.thumbnail_url) return asset.thumbnail_url;
  if (!asset.thumbnail_sha256) return '';
  const cacheKey = `${state.profile}:${asset.thumbnail_sha256}:${asset.thumbnail_mime_type || ''}`;
  if (!thumbObjectUrls.has(cacheKey)) {
    const path = `/api/object?${qs({profile: state.profile, sha256: asset.thumbnail_sha256})}`;
    thumbObjectUrls.set(cacheKey, apiBlob(path, asset.thumbnail_mime_type).then(blob => URL.createObjectURL(blob)));
  }
  return thumbObjectUrls.get(cacheKey);
}

async function loadThumbnailImage(img, asset) {
  const url = await thumbnailObjectUrl(asset);
  if (!url || !img.isConnected) return;
  img.src = url;
}

function hydrateGridThumbnails() {
  $('assetGrid').querySelectorAll('img[data-thumb-key]').forEach(img => {
    const asset = state.assets.find(item => assetKey(item) === img.dataset.thumbKey);
    if (asset) loadThumbnailImage(img, asset).catch(() => img.remove());
  });
}

function thumbnailAssetForVersion(asset, thumbnails, version) {
  const record = thumbnails.find(thumbnail =>
    String(thumbnail.version) === String(version) &&
    thumbnail.department_key?.asset_key?.category === asset.category &&
    thumbnail.department_key?.asset_key?.asset_code === asset.asset_code &&
    thumbnail.department_key?.department === asset.department);
  if (!record) return asset;
  return {
    ...asset,
    thumbnail_url: null,
    thumbnail_sha256: record.sha256,
    thumbnail_mime_type: record.mime_type
  };
}

function renderDetailThumbnail(asset, thumbnails) {
  const thumbAsset = thumbnailAssetForVersion(asset, thumbnails, $('versionSelect').value);
  if (!thumbAsset.thumbnail_url && !thumbAsset.thumbnail_sha256) {
    $('detailPreview').innerHTML = '<span>NO THUMBNAIL</span>';
    return;
  }
  $('detailPreview').innerHTML = '<img alt="" onerror="this.remove()">';
  loadThumbnailImage($('detailPreview').querySelector('img'), thumbAsset).catch(() => {
    $('detailPreview').innerHTML = '<span>NO THUMBNAIL</span>';
  });
}

function renderTakeLog(data) {
  const selected = $('versionSelect').value;
  const currentStatus = data.current_status || {};
  $('versionList').innerHTML = data.versions.slice().reverse().map(v => {
    const isCurrent = v.version === currentStatus.current;
    const isLatest = v.version === currentStatus.latest;
    const isSelected = String(v.version) === selected;
    const date = (v.created_at || '').slice(0, 10);
    const tag = isCurrent
      ? '<span class="tag pin">PIN</span>'
      : (isLatest ? '<span class="tag latest">LATEST</span>' : '');
    return `<div class="take${isCurrent ? ' current' : ''}${isSelected ? ' selected' : ''}" data-version="${esc(v.version)}">
      <span class="v">${fmtVersion(v.version)}</span>
      <span class="meta">${v.file_count} files · ${humanBytes(v.total_bytes)}${date ? ' · ' + esc(date) : ''}</span>
      ${tag}
    </div>`;
  }).join('');
  $('versionList').querySelectorAll('.take').forEach(row => {
    row.addEventListener('click', () => {
      $('versionSelect').value = row.dataset.version;
      state.selectedVersion = row.dataset.version;
      renderDetailThumbnail(state.selected, data.thumbnails || []);
      updateAssetUriField();
      renderTakeLog(data);
      loadManifest(row.dataset.version).catch(showError);
    });
  });
}

function renderWips() {
  const wips = state.wips || [];
  $('wipSummary').textContent = wips.length
    ? `${wips.length} write${wips.length === 1 ? '' : 's'}`
    : 'empty';
  const head = wips.length ? wips[wips.length - 1].seq : null;
  $('wipList').innerHTML = wips.slice().reverse().map(w => {
    const isHead = w.seq === head;
    const date = (w.created_at || '').replace('T', ' ').slice(0, 16);
    const tag = isHead ? '<span class="tag head">HEAD</span>' : '';
    return `<div class="take wip" data-seq="${esc(w.seq)}" title="?v=wip resolves the newest WIP — click to copy the URI">
      <span class="v">#${esc(w.seq)}</span>
      <span class="meta">${w.file_count} files · ${humanBytes(w.total_bytes)}${date ? ' · ' + esc(date) : ''}</span>
      <span class="wip-actions">${tag}<button type="button" class="promote-btn" data-seq="${esc(w.seq)}">PROMOTE</button></span>
    </div>`;
  }).join('');
  $('wipList').querySelectorAll('.take.wip').forEach(row => {
    row.addEventListener('click', async () => {
      const asset = state.selected;
      if (!asset) return;
      const uri = assetUri(asset, 'wip');
      const copied = await copyText(uri);
      status(copied ? `Copied ${uri}` : `Clipboard blocked. ${uri}`, copied ? 'ok' : 'err');
    });
  });
  $('wipList').querySelectorAll('.promote-btn').forEach(button => {
    button.addEventListener('click', event => {
      event.stopPropagation();
      promoteWip(Number(button.dataset.seq)).catch(showError);
    });
  });
}

// Promote runs the same validation gate as `ads publish promote`; the server
// rejects with the error list when references would break after publish.
async function promoteWip(seq) {
  const asset = state.selected;
  if (!asset) return;
  status(`Promoting WIP #${seq}…`);
  const result = await api('/api/promote', {method: 'POST', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, wip_seq: seq})});
  const outcome = result.outcome;
  const warnings = (result.validation && result.validation.warnings) || [];
  await refreshSelection();
  const verb = outcome.created ? 'Promoted to' : 'Already published as';
  const suffix = warnings.length ? ` (${warnings.length} validation warning${warnings.length === 1 ? '' : 's'})` : '';
  status(`${verb} ${fmtVersion(outcome.version)}${suffix}`, 'ok');
}

const USD_EXTENSIONS = ['usd', 'usda', 'usdc', 'usdz'];
const TEXTURE_EXTENSIONS = ['tx', 'rat', 'exr', 'tif', 'tiff', 'png', 'jpg', 'jpeg', 'tga', 'bmp', 'hdr', 'pic', 'tex'];

function fileKindHue(path) {
  const ext = String(path).split('.').pop().toLowerCase();
  if (USD_EXTENSIONS.includes(ext)) return 38;
  if (TEXTURE_EXTENSIONS.includes(ext)) return 175;
  return null;
}

async function loadManifest(version) {
  const asset = state.selected;
  if (!asset || version === '' || version === null || version === undefined) return;
  const cacheKey = `${state.profile}/${assetKey(asset)}@${version}`;
  let info = state.manifestCache.get(cacheKey);
  if (!info) {
    const params = {profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version};
    info = await api(`/api/version?${qs(params)}`);
    state.manifestCache.set(cacheKey, info);
  }
  renderManifest(info, version);
}

function renderManifest(info, version) {
  const entries = (info.manifest && info.manifest.entries) || [];
  const total = entries.reduce((sum, entry) => sum + entry.size, 0);
  $('manifestSummary').textContent = entries.length
    ? `${entries.length} file${entries.length === 1 ? '' : 's'} · ${humanBytes(total)}`
    : 'empty';
  $('manifestList').innerHTML = entries.map(entry => {
    const hue = fileKindHue(entry.relative_path);
    const dot = hue === null
      ? '<span class="file-dot neutral"></span>'
      : `<span class="file-dot" style="--dh:${hue}"></span>`;
    return `<div class="file-row" data-path="${esc(entry.relative_path)}" title="sha256 ${esc(entry.sha256)} — click to copy ads:// URI">
      ${dot}
      <span class="file-path">${esc(entry.relative_path)}</span>
      <span class="file-size">${humanBytes(entry.size)}</span>
    </div>`;
  }).join('');
  $('manifestList').querySelectorAll('.file-row').forEach(row => {
    row.addEventListener('click', async () => {
      const asset = state.selected;
      if (!asset) return;
      const uri = `ads://${asset.category}/${asset.asset_code}/${asset.department}/${row.dataset.path}?v=${encodeURIComponent(version)}`;
      const copied = await copyText(uri);
      status(copied ? `Copied ${uri}` : `Clipboard blocked. ${uri}`, copied ? 'ok' : 'err');
    });
  });
}

async function refreshSelection() {
  await loadAssets();
  const asset = state.selected;
  if (!asset) return;
  const refreshed = state.assets.find(item => assetKey(item) === assetKey(asset));
  if (refreshed) await selectAsset(refreshed);
}

async function setCurrent() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  await api('/api/current', {method: 'PUT', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version})});
  await refreshSelection();
  status(`Pinned current to ${fmtVersion(version)}`, 'ok');
}

async function resetCurrent() {
  const asset = state.selected;
  if (!asset) return;
  await api('/api/current', {method: 'PUT', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, reset: true})});
  await refreshSelection();
  status('Current follows latest', 'ok');
}

async function pullToWorkspace() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  status('Pulling…');
  await api('/api/pull', {method: 'POST', body: JSON.stringify({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version, force: $('forcePull').checked})});
  status(`Pulled ${fmtVersion(version)} to workspace`, 'ok');
}

async function copyThumbUrl() {
  const asset = state.selected;
  if (!asset) return;
  const version = $('versionSelect').value;
  const url = await api(`/api/thumbnail-url?${qs({profile: state.profile, category: asset.category, asset_code: asset.asset_code, department: asset.department, version})}`);
  const copied = await copyText(url);
  status(copied ? 'Thumbnail URL copied' : `Clipboard blocked. Thumbnail URL: ${url}`, copied ? 'ok' : 'err');
}

function assetUri(asset, version) {
  const base = `ads://${asset.category}/${asset.asset_code}/${asset.department}/${asset.asset_code}.usd`;
  return version ? `${base}?v=${encodeURIComponent(version)}` : base;
}

function updateAssetUriField() {
  const asset = state.selected;
  if (!asset) return;
  $('assetUriInput').value = assetUri(asset, $('versionSelect').value);
}

async function copyAssetUri() {
  const asset = state.selected;
  if (!asset) return;
  updateAssetUriField();
  const uri = $('assetUriInput').value;
  const copied = await copyText(uri);
  status(copied ? 'ADS URI copied' : `Clipboard blocked. ADS URI: ${uri}`, copied ? 'ok' : 'err');
}

async function uploadThumbnail() {
  const asset = state.selected;
  const file = $('thumbnailInput').files[0];
  if (!asset || !file) return;
  const form = new FormData();
  form.set('profile', state.profile);
  form.set('category', asset.category);
  form.set('asset_code', asset.asset_code);
  form.set('department', asset.department);
  form.set('version', $('versionSelect').value);
  form.set('file', file);
  status('Uploading thumbnail…');
  await api('/api/thumbnails', {method: 'POST', body: form});
  await refreshSelection();
  status('Thumbnail uploaded', 'ok');
}

function assetKey(asset) {
  return `${asset.category}/${asset.asset_code}/${asset.department}`;
}

function showError(error) {
  status(error.message || String(error), 'err');
}

let searchTimer = null;
$('searchInput').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => loadAssets().catch(showError), 250);
});

$('authForm').addEventListener('submit', event => {
  event.preventDefault();
  state.token = $('tokenInput').value.trim();
  sessionStorage.setItem('adsToken', state.token);
  init().catch(showError);
});
$('profileSelect').addEventListener('change', () => {
  state.profile = $('profileSelect').value;
  state.category = '';
  state.department = '';
  loadAssets().catch(showError);
});
$('refreshButton').addEventListener('click', () => loadAssets().catch(showError));
$('logoutButton').addEventListener('click', () => { sessionStorage.removeItem('adsToken'); location.reload(); });
$('setCurrentButton').addEventListener('click', () => setCurrent().catch(showError));
$('resetCurrentButton').addEventListener('click', () => resetCurrent().catch(showError));
$('pullButton').addEventListener('click', () => pullToWorkspace().catch(showError));
$('versionSelect').addEventListener('change', () => {
  state.selectedVersion = $('versionSelect').value;
  updateAssetUriField();
  if (state.selected) {
    renderDetailThumbnail(state.selected, state.thumbnails || []);
    renderTakeLog({versions: state.versions, current_status: state.lastCurrentStatus || {}, thumbnails: state.thumbnails || []});
    loadManifest(state.selectedVersion).catch(showError);
  }
});
$('copyAssetUriButton').addEventListener('click', () => copyAssetUri().catch(showError));
$('copyThumbUrlButton').addEventListener('click', () => copyThumbUrl().catch(showError));
$('thumbnailInput').addEventListener('change', () => uploadThumbnail().catch(showError));

init().catch(showError);
"#;
