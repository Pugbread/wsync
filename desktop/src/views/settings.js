// views/settings.js — Design 8.2's settings surface.
//
// Real end to end: Appearance (the theme engine), the Projects folder (the
// native picker plus persistence), the served-projects table, and **Secrets**
// — the Open Cloud API key, held in the OS keychain with a 0600 file fallback.
// What is left calls its real host command, receives `not_implemented`, and
// says so in place: a settings screen that silently accepts input it cannot
// store would be worse than one that admits the gap.

import { el, icon, plural, relativeTime } from "./dom.js";

export function mountSettings(root, api) {
  const scroll = el("div", { class: "view-scroll" });
  const layout = el("div", { class: "settings-layout" });

  scroll.append(
    el(
      "div",
      { class: "view-head" },
      el(
        "div",
        {},
        el("h1", { class: "view-title", text: "Settings" }),
        el("p", {
          class: "view-sub",
          text: "Appearance, the projects folder, the API key and the Studio plugin apply immediately. Sections marked below are still waiting on the pieces they need.",
        }),
      ),
    ),
    layout,
  );
  root.append(scroll);

  // ------------------------------------------------------------ appearance --

  const themeGrid = el("div", { class: "theme-grid" });

  function renderThemes() {
    const current = api.getAppearanceTheme();
    themeGrid.replaceChildren(
      ...api.themeOptions.map((option) =>
        el(
          "button",
          {
            class: "theme-option",
            type: "button",
            "aria-pressed": current === option.id ? "true" : "false",
            on: {
              click: () => {
                api.setAppearanceTheme(option.id);
                renderThemes();
                api.setStatus(`Appearance: ${option.label}.`, "ok");
              },
            },
          },
          el(
            "span",
            { class: "theme-preview", data: { preview: option.id }, "aria-hidden": "true" },
            el("span", { class: "theme-preview-rail" }),
            el(
              "span",
              { class: "theme-preview-body" },
              el("span", { class: "theme-preview-line" }),
              el("span", { class: "theme-preview-line" }),
              el("span", { class: "theme-preview-line" }),
            ),
          ),
          el("span", { class: "theme-option-label", text: option.label }),
          el("span", { class: "theme-option-desc", text: option.description }),
        ),
      ),
    );
  }

  renderThemes();
  layout.append(section("Appearance", "Applies immediately and is remembered.", themeGrid));

  // -------------------------------------------------------- projects folder --
  //
  // Design 8.4: authorizing a folder is not a preference, it is what turns the
  // broker on. So both buttons call a host command that does the whole thing —
  // persist the root *and* start or stop the listener — and this section only
  // ever reports what came back.

  const rootValue = el("span", { class: "kv-value path" });
  const brokerLine = el("div", { class: "row-sub" });
  const brokerChip = el("span", { class: "chip", text: "Off" });
  const rootBusy = { value: false };

  const rootClearButton = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: clearProjectsRoot } },
    "Clear",
  );
  const rootRevealButton = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: revealProjectsRoot } },
    "Open",
  );
  const rootChooseButton = el(
    "button",
    { class: "btn", type: "button", on: { click: chooseProjectsRoot } },
    icon("folder", 13),
    "Choose folder",
  );

  function renderProjectsRoot() {
    const value = api.getState().projectsRoot;
    const broker = api.getBrokerStatus();

    rootValue.textContent = value ?? "Not set — Studio cannot create projects";
    rootValue.title = value ?? "";
    rootClearButton.hidden = !value;
    rootRevealButton.hidden = !value;
    rootClearButton.disabled = rootBusy.value;
    rootRevealButton.disabled = rootBusy.value;
    rootChooseButton.disabled = rootBusy.value;
    rootChooseButton.textContent = rootBusy.value ? "Working…" : value ? "Change folder" : "Choose folder";
    if (!rootBusy.value) rootChooseButton.prepend(icon("folder", 13));

    // The broker's own words: "ready on port N", or why it is not.
    brokerLine.textContent = broker?.running
      ? `Ready on port ${broker.port} · 127.0.0.1 only · GET /hello, POST /projects/init`
      : (broker?.detail ?? "Off — authorize a folder.");
    brokerChip.textContent = broker?.running ? `Port ${broker.port}` : "Off";
    // Authorized but not listening is a state worth flagging; nothing
    // authorized is simply off.
    brokerChip.className = `chip ${broker?.running ? "chip-ok" : value ? "chip-warn" : ""}`.trim();
  }

  async function withRootBusy(work, failureTitle) {
    rootBusy.value = true;
    renderProjectsRoot();
    try {
      await work();
      renderProjectsRoot();
    } catch (error) {
      if (!error?.isCancelled) {
        api.toast(failureTitle, {
          kind: error?.isHostless ? "warn" : "err",
          body: error?.message ?? String(error),
        });
      }
    } finally {
      rootBusy.value = false;
      renderProjectsRoot();
    }
  }

  async function chooseProjectsRoot() {
    await withRootBusy(async () => {
      const answer = await api.setProjectsRoot();
      api.setStatus(
        answer?.broker?.running
          ? `Studio can create projects in ${answer.name} — broker on port ${answer.broker.port}.`
          : "Projects folder set, but the broker is not listening.",
        answer?.broker?.running ? "ok" : "warn",
      );
    }, "Could not set the projects folder");
  }

  async function clearProjectsRoot() {
    await withRootBusy(async () => {
      await api.clearProjectsRoot();
      api.setStatus("Projects folder cleared — the broker is off.");
    }, "Could not clear the projects folder");
  }

  async function revealProjectsRoot() {
    try {
      await api.revealProjectsRoot();
    } catch (error) {
      api.toast("Could not open that folder", {
        kind: error?.isHostless ? "warn" : "err",
        body: error?.message ?? String(error),
      });
    }
  }

  renderProjectsRoot();
  void api.refreshBrokerStatus().then(renderProjectsRoot);

  layout.append(
    section(
      "Projects folder",
      "Where projects created from Studio are written. Authorizing a folder is what allows Studio-triggered creation at all — the broker listens only while one is set.",
      el(
        "div",
        { class: "row" },
        el("div", { class: "row-copy" }, el("div", { class: "row-title", text: "Authorized folder" }), rootValue),
        el("div", { class: "row-actions" }, rootRevealButton, rootClearButton, rootChooseButton),
      ),
      el(
        "div",
        { class: "row" },
        el(
          "div",
          { class: "row-copy" },
          el("div", { class: "row-title", text: "Project broker" }),
          brokerLine,
        ),
        el("div", { class: "row-actions" }, brokerChip),
      ),
      el("p", {
        class: "field-hint",
        text: "WSync creates exactly one direct child of this folder per request, from a slug it derives itself. Studio never supplies a path, and a name already taken by a symbolic link is refused rather than followed.",
      }),
    ),
  );

  // ---------------------------------------------------------------- secrets --

  // The host answers with a *status*, never the key: `{present, masked, store,
  // note}`. So this section shows what is true — that a key is set, which store
  // holds it, and the last few characters so two keys can be told apart — and
  // has no way to show more even if it wanted to.
  const SECRET_NAME = "openCloudApiKey";

  const secretInput = el("input", {
    class: "input",
    type: "password",
    placeholder: "Open Cloud API key",
    autocomplete: "off",
    spellcheck: "false",
    "aria-label": "Open Cloud API key",
    on: { input: () => renderSecretButtons() },
  });
  const secretNotice = el("div", { hidden: true });
  const secretStatus = el("div", { class: "row-sub", text: "Checking…" });
  const secretClearButton = el(
    "button",
    { class: "btn", type: "button", disabled: true, on: { click: clearSecret } },
    "Clear",
  );
  const secretSaveButton = el(
    "button",
    { class: "btn btn-primary", type: "button", disabled: true, on: { click: saveSecret } },
    "Save",
  );

  /** The last status the host gave us, or null before the first answer. */
  let secret = null;
  let secretBusy = false;

  const STORE_LABEL = {
    keychain: "macOS keychain",
    file: "0600 file store",
  };

  function renderSecret() {
    if (secret === null) {
      secretStatus.textContent = "Checking…";
    } else if (secret.present) {
      const where = STORE_LABEL[secret.store] ?? "an unknown store";
      secretStatus.textContent = `Set ${secret.masked ?? ""} · ${where}`.trim();
    } else {
      secretStatus.textContent = "Not set";
    }
    renderSecretButtons();
  }

  function renderSecretButtons() {
    secretSaveButton.disabled = secretBusy || secretInput.value.trim() === "";
    secretClearButton.disabled = secretBusy || secret === null || !secret.present;
    secretSaveButton.textContent = secretBusy ? "Working…" : "Save";
  }

  /**
   * One error renderer for all three calls, because the three failures a user
   * can actually hit read very differently: no host at all (browser preview),
   * a capability that is not built (`not_implemented`), and a store that
   * refused. Collapsing them into "error" would hide which one happened.
   */
  function reportSecretError(error) {
    if (error?.isHostless) {
      secretStatus.textContent = "Needs the WSync desktop host";
      showNotice(
        secretNotice,
        "warn",
        "Secrets are held by the desktop host, so they cannot be read or written in a browser preview.",
      );
      return;
    }
    showNotice(
      secretNotice,
      error?.isNotImplemented ? "warn" : "danger",
      error?.message ?? String(error),
    );
  }

  async function refreshSecret() {
    try {
      secret = await api.host.secretGet(SECRET_NAME);
      renderSecret();
      // A note on a *read* is the host explaining a degraded answer — a
      // keychain that would not open, say. Worth showing without a click.
      if (secret.note) showNotice(secretNotice, "warn", secret.note);
    } catch (error) {
      secret = null;
      renderSecretButtons();
      reportSecretError(error);
    }
  }

  async function saveSecret() {
    const value = secretInput.value;
    if (!value.trim()) {
      showNotice(secretNotice, "warn", "Enter a key before saving.");
      return;
    }
    secretBusy = true;
    renderSecretButtons();
    try {
      secret = await api.host.secretSet(SECRET_NAME, value);
      // Cleared on success only: a failed save should not also lose what was
      // typed, which for a pasted API key means fetching it again.
      secretInput.value = "";
      renderSecret();
      showNotice(
        secretNotice,
        secret.note ? "warn" : "ok",
        secret.note ??
          `Stored in the ${STORE_LABEL[secret.store] ?? "secret store"}. WSync keeps the key; this screen only ever shows ${secret.masked}.`,
      );
    } catch (error) {
      reportSecretError(error);
    } finally {
      secretBusy = false;
      renderSecretButtons();
    }
  }

  async function clearSecret() {
    secretBusy = true;
    renderSecretButtons();
    try {
      secret = await api.host.secretClear(SECRET_NAME);
      renderSecret();
      showNotice(
        secretNotice,
        secret.note ? "warn" : "ok",
        secret.note ?? "Cleared from the keychain and the file store.",
      );
    } catch (error) {
      reportSecretError(error);
    } finally {
      secretBusy = false;
      renderSecretButtons();
    }
  }

  renderSecret();
  void refreshSecret();

  layout.append(
    section(
      "Secrets",
      "Stored in the OS keychain, with a 0600 file fallback. The key is never put on a command line, never logged, and never returned to this screen — only its last few characters are.",
      el(
        "div",
        { class: "row" },
        el(
          "div",
          { class: "row-copy" },
          el("div", { class: "row-title", text: "Open Cloud API key" }),
          secretStatus,
        ),
        el("div", { class: "row-actions" }, secretClearButton),
      ),
      el(
        "div",
        { class: "row" },
        el("div", { class: "row-copy", style: "flex:1" }, secretInput),
        el("div", { class: "row-actions" }, secretSaveButton),
      ),
      secretNotice,
    ),
  );

  // ---------------------------------------------------------- studio plugin --
  //
  // The host does the resolving, the verifying and the copying; this section
  // reports what came back. Three failures read very differently and each gets
  // its own state, because collapsing them would hide the one thing the user
  // has to do next:
  //
  //   * `integrity`   — the artifact is not what its manifest describes.
  //                     Retrying installs the same wrong bytes, so the button
  //                     turns into "rebuild it" rather than "try again".
  //   * `unavailable` — there is no artifact at all. The build command is in
  //                     the host's message, and a "Choose file" affordance
  //                     appears for the case where one exists elsewhere.
  //   * everything else — reported verbatim.

  const pluginStatusLine = el("div", { class: "row-sub", text: "Checking…" });
  const pluginChip = el("span", { class: "chip chip-plain", text: "—" });
  const pluginNotice = el("div", { hidden: true });
  const pluginWarning = el("div", { hidden: true });

  const pluginRevealButton = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: revealPluginsFolder } },
    "Open folder",
  );
  const pluginPickButton = el(
    "button",
    { class: "btn btn-sm", type: "button", hidden: true, on: { click: () => installPlugin({ pick: true }) } },
    "Choose WSync.rbxm…",
  );
  const pluginInstallButton = el(
    "button",
    { class: "btn", type: "button", on: { click: () => installPlugin() } },
    "Install plugin",
  );

  /** The last status the host gave us, or null before the first answer. */
  let plugin = null;
  /** Why there is no status, when the call itself could not be made. */
  let pluginUnreachable = null;
  let pluginBusy = false;

  const PLUGIN_SOURCE_LABEL = {
    resources: "from this app",
    env: "from WSYNC_PLUGIN_ARTIFACT",
    "dev-tree": "from the checkout",
    picked: "from the file you chose",
  };

  /** Which served project the plugin's protocol is compared against. */
  function pluginProjectId() {
    const served = api.servedProjectIds();
    if (served.length === 0) return null;
    const active = api.getState().activeProjectId;
    return served.includes(active) ? active : served[0];
  }

  function renderPlugin() {
    pluginInstallButton.disabled = pluginBusy;
    pluginPickButton.disabled = pluginBusy;
    pluginRevealButton.disabled = pluginBusy;

    if (pluginBusy) {
      pluginInstallButton.textContent = "Installing…";
    } else {
      pluginInstallButton.textContent = plugin?.installed ? "Reinstall" : "Install plugin";
    }

    if (plugin === null) {
      // Before the first answer, or after one that never arrived. The two are
      // not the same thing, and "Checking…" forever would be a lie about the
      // second.
      pluginStatusLine.textContent = pluginUnreachable ?? "Checking…";
      pluginStatusLine.title = "";
      pluginChip.textContent = "—";
      pluginChip.className = "chip chip-plain";
      pluginWarning.hidden = true;
      if (pluginUnreachable) {
        pluginInstallButton.disabled = true;
        pluginRevealButton.disabled = true;
      }
      return;
    }

    if (!plugin.pluginsDir) {
      // No Studio on this platform, so there is nothing to install into and
      // nothing to reveal. Saying so beats a button that always fails.
      pluginStatusLine.textContent = plugin.note ?? "No Roblox plugins folder on this platform";
      pluginStatusLine.title = "";
      pluginChip.textContent = "Unavailable";
      pluginChip.className = "chip chip-plain";
      pluginInstallButton.disabled = true;
      pluginRevealButton.disabled = true;
      pluginPickButton.hidden = true;
      pluginWarning.hidden = true;
      return;
    }

    if (!plugin.installed) {
      pluginStatusLine.textContent = `Not installed · ${plugin.pluginsDir}`;
      pluginStatusLine.title = plugin.pluginsDir;
      pluginChip.textContent = "Not installed";
      pluginChip.className = "chip chip-plain";
    } else {
      // Version first when it is known, because that is the question being
      // asked. The build sha and time are what actually tell two builds of the
      // same version apart — the version string alone cannot.
      const built = formatBuilt(plugin.builtAt);
      const parts = [
        plugin.pluginVersion ? `Installed ${plugin.pluginVersion}` : "Installed",
        plugin.protocolVersion === null || plugin.protocolVersion === undefined
          ? null
          : `protocol ${plugin.protocolVersion}`,
        plugin.sha256 ? `build ${plugin.sha256.slice(0, 8)}` : null,
        built ? `built ${built}` : null,
        formatBytes(plugin.size),
        plugin.modifiedAt ? `installed ${relativeTime(plugin.modifiedAt)}` : null,
      ].filter(Boolean);
      pluginStatusLine.textContent = parts.join(" · ");
      pluginStatusLine.title = plugin.path ?? "";
      pluginChip.textContent = plugin.verified ? "Verified" : "Unverified";
      pluginChip.className = `chip ${plugin.verified ? "chip-ok" : "chip-warn"}`;
    }

    // The protocol mismatch is the failure that looks like nothing happening:
    // Studio connects, the daemon rejects the hello, and the user sees a dead
    // panel. It gets its own line rather than sharing the note.
    if (plugin.warning) showNotice(pluginWarning, "danger", plugin.warning);
    else pluginWarning.hidden = true;

    // A note on a *status* explains a degraded answer — an unmatched file, a
    // stale single-file plugin still in the folder. Worth showing unprompted,
    // but never over the top of a fresh install result.
    if (plugin.note && !pluginBusy && pluginNotice.hidden) {
      showNotice(pluginNotice, "warn", plugin.note);
    }
  }

  async function refreshPluginStatus() {
    try {
      plugin = await api.host.pluginStatus(pluginProjectId());
      pluginUnreachable = null;
    } catch (error) {
      plugin = null;
      // `plugin_status` is written not to throw, so anything landing here is
      // the boundary itself failing — no host at all, or an IPC error. Either
      // way the section has nothing true to report and says so, once.
      pluginUnreachable = error?.isHostless
        ? "Needs the WSync desktop host"
        : (error?.message ?? String(error));
    }
    renderPlugin();
  }

  async function installPlugin(options) {
    pluginBusy = true;
    pluginNotice.hidden = true;
    renderPlugin();
    try {
      const report = await api.host.pluginInstall(options);
      pluginPickButton.hidden = true;
      const where = PLUGIN_SOURCE_LABEL[report.source] ?? report.source;
      const builtAt = formatBuilt(report.builtAt);
      const details = [
        report.protocolVersion === null || report.protocolVersion === undefined
          ? null
          : `protocol ${report.protocolVersion}`,
        report.sha256 ? `build ${report.sha256.slice(0, 8)}` : null,
        builtAt ? `built ${builtAt}` : null,
      ].filter(Boolean);
      const identity = `${report.pluginVersion ? `WSync ${report.pluginVersion}` : "the plugin"}${
        details.length ? ` (${details.join(", ")})` : ""
      }`;
      showNotice(
        pluginNotice,
        report.verified ? "ok" : "warn",
        `Installed ${identity} ${where} to ${report.path}. Studio reloads local plugins on its own — restart Studio only if it still shows the old build.${
          report.note ? ` ${report.note}` : ""
        }`,
      );
      api.setStatus(
        report.pluginVersion ? `Studio plugin ${report.pluginVersion} installed.` : "Studio plugin installed.",
        report.verified ? "ok" : "warn",
      );
    } catch (error) {
      reportPluginError(error);
    } finally {
      // Clearing busy *before* the refresh, so the one `renderPlugin` it does
      // draws the settled state rather than a second "Installing…" frame. The
      // notice set above survives it — `renderPlugin` only fills an empty one.
      pluginBusy = false;
      await refreshPluginStatus();
    }
  }

  function reportPluginError(error) {
    if (error?.isCancelled) return;
    if (error?.isHostless) {
      showNotice(
        pluginNotice,
        "warn",
        "Installing the Studio plugin needs the WSync desktop host; it cannot run in a browser preview.",
      );
      return;
    }
    if (error?.isIntegrity) {
      // Nothing was written. Retrying would install the same wrong bytes, so
      // the only honest next step is a rebuild.
      showNotice(pluginNotice, "danger", error.message);
      return;
    }
    if (error?.isUnavailable) {
      // The host's message already carries `node plugin/scripts/build.mjs`.
      showNotice(pluginNotice, "warn", error.message);
      // Offering to pick a file only makes sense where there is somewhere to
      // install it: `unavailable` also covers "this platform has no Roblox
      // plugins folder", and the status line is what knows which it was.
      pluginPickButton.hidden = !plugin?.pluginsDir;
      return;
    }
    showNotice(pluginNotice, error?.isNotImplemented ? "warn" : "danger", error?.message ?? String(error));
  }

  async function revealPluginsFolder() {
    try {
      await api.host.pluginsDirReveal();
    } catch (error) {
      showNotice(pluginNotice, error?.isHostless ? "warn" : "danger", error?.message ?? String(error));
    }
  }

  renderPlugin();
  void refreshPluginStatus();

  layout.append(
    section(
      "Studio plugin",
      "Installs WSync.rbxm into the Roblox plugins folder, after checking it against the sha256 its build published. A plugin whose protocol does not match the running daemon is flagged here.",
      el(
        "div",
        { class: "row" },
        el(
          "div",
          { class: "row-copy" },
          el("div", { class: "row-title", text: "Plugin status" }),
          pluginStatusLine,
        ),
        el("div", { class: "row-actions" }, pluginChip, pluginRevealButton, pluginPickButton, pluginInstallButton),
      ),
      pluginWarning,
      pluginNotice,
    ),
  );

  // -------------------------------------------------------- served projects --

  const servedBody = el("div", {});

  function renderServed() {
    const served = api.servedProjectIds();
    if (served.length === 0) {
      servedBody.replaceChildren(
        el("p", {
          class: "field-hint",
          text: "No project is being served. Turn a project on from the Projects view.",
        }),
      );
      return;
    }
    servedBody.replaceChildren(
      ...served.map((id) => {
        const project = api.getProject(id);
        const session = api.getDaemonSession(id);
        const link = api.getLinkState();
        const where = session?.ok
          ? `port ${session.port} · pid ${session.pid}${session.managed === false ? " · owned elsewhere" : ""}`
          : "not running";
        return el(
          "div",
          { class: "row" },
          el(
            "div",
            { class: "row-copy" },
            el("div", { class: "row-title", text: project?.name ?? id }),
            el("div", { class: "row-sub", text: where }),
          ),
          el(
            "div",
            { class: "row-actions" },
            link?.projectId === id
              ? el("span", { class: "chip chip-plain", text: link.state })
              : null,
            el(
              "button",
              {
                class: "btn btn-sm",
                type: "button",
                on: {
                  click: async (event) => {
                    event.target.disabled = true;
                    await api.stopProject(id);
                    renderServed();
                  },
                },
              },
              "Stop",
            ),
          ),
        );
      }),
    );
  }

  renderServed();
  layout.append(section("Served projects", "One daemon per project, loopback only.", servedBody));

  // -------------------------------------------------------- engine defaults --

  layout.append(
    section(
      "Sync engine defaults",
      "Workspace-level defaults written to ~/.wsync/config.toml. The editor arrives with the engine's config surface.",
      el("p", { class: "field-hint", text: "Not editable yet." }),
    ),
  );

  // ------------------------------------------------------------- developer --

  layout.append(
    section(
      "Developer",
      "Layout affordances for work in progress. These render fixtures; they never touch a project.",
      el(
        "div",
        { class: "row" },
        el(
          "div",
          { class: "row-copy" },
          el("div", { class: "row-title", text: "Divergence modal" }),
          el("div", { class: "row-sub", text: "Opens the overwrite flow against its sample divergence set." }),
        ),
        el(
          "div",
          { class: "row-actions" },
          el(
            "button",
            { class: "btn", type: "button", on: { click: () => api.openOverwriteModal({ fixture: true }) } },
            "Preview",
          ),
        ),
      ),
    ),
  );

  // ------------------------------------------------------------------ about --
  //
  // Two builds are in play at once and they are versioned separately: this
  // app, and the engine binary serving a project. A bug report that names only
  // one of them is half a bug report, so the daemon's own identity — the
  // `version`, `commit`, `dirty` and `protocol` from its `/hello` — is shown
  // beside the app's and can be copied in one click.

  const info = api.appInfo();

  const daemonIdentity = el("span", { class: "about-daemon-text mono", text: "—" });
  const daemonCopyButton = el(
    "button",
    { class: "btn btn-sm", type: "button", hidden: true, on: { click: copyDaemonIdentity } },
    "Copy",
  );
  /** The exact string the copy button puts on the clipboard, or null. */
  let daemonIdentityText = null;

  function renderDaemonIdentity(status) {
    // `/hello` is the daemon's own answer; the host passes it through
    // untouched on `daemonStatus`, so nothing here is second-hand.
    const hello = status?.hello ?? null;
    if (!hello) {
      daemonIdentityText = null;
      // The first line only: an unreachable daemon's `detail` carries its
      // stderr tail, which belongs in the Projects view's diagnostic, not in a
      // one-line About row. The whole thing stays reachable as the tooltip.
      const detail = String(status?.detail ?? "").split("\n")[0];
      daemonIdentity.textContent = status
        ? detail || "not answering"
        : "no project is being served";
      daemonIdentity.title = status?.detail ?? "";
      daemonIdentity.classList.remove("mono");
      daemonCopyButton.hidden = true;
      return;
    }

    daemonIdentityText = [
      `daemon ${hello.version ?? "?"}`,
      hello.protocol === undefined || hello.protocol === null ? null : `protocol ${hello.protocol}`,
      hello.commit ?? null,
      hello.dirty === true ? "dirty" : null,
    ]
      .filter(Boolean)
      .join(" · ");
    daemonIdentity.textContent = daemonIdentityText;
    daemonIdentity.classList.add("mono");
    daemonIdentity.title = hello.writesLog ? `writes log: ${hello.writesLog}` : "";
    daemonCopyButton.hidden = false;
  }

  async function copyDaemonIdentity() {
    if (!daemonIdentityText) return;
    const copied = await copyText(daemonIdentityText);
    api.setStatus(copied ? "Daemon build copied." : "Could not copy to the clipboard.", copied ? "ok" : "warn");
  }

  /** Re-read `/hello` through the same status path the Projects view polls. */
  async function refreshDaemonIdentity() {
    const projectId = pluginProjectId();
    if (!projectId) {
      renderDaemonIdentity(null);
      return;
    }
    renderDaemonIdentity(await api.daemonStatus(projectId));
  }

  renderDaemonIdentity(null);
  void refreshDaemonIdentity();

  layout.append(
    section(
      "About",
      null,
      el(
        "div",
        { class: "kv" },
        el("span", { class: "kv-key", text: "Version" }),
        el("span", { class: "kv-value", text: info?.version ?? "—" }),
        el("span", { class: "kv-key", text: "Protocol" }),
        el("span", { class: "kv-value", text: info?.protocol ?? "—" }),
        el("span", { class: "kv-key", text: "Daemon" }),
        el("span", { class: "kv-value about-daemon" }, daemonIdentity, daemonCopyButton),
        el("span", { class: "kv-key", text: "Platform" }),
        el("span", { class: "kv-value", text: info ? `${info.platform} · ${info.target}` : "browser preview" }),
        el("span", { class: "kv-key", text: "Updates" }),
        el("span", {
          class: "kv-value",
          text: info?.updaterConfigured
            ? "Signed updates enabled"
            : "Not configured — this build has no updater key",
        }),
        el("span", { class: "kv-key", text: "Data" }),
        el("span", { class: "kv-value path", text: info?.dataDir ?? "—", title: info?.dataDir ?? "" }),
        el("span", { class: "kv-key", text: "State" }),
        el("span", {
          class: "kv-value path",
          text: api.persistence() === "host" ? (info?.stateFile ?? "—") : "in memory only",
          title: info?.stateFile ?? "",
        }),
        el("span", { class: "kv-key", text: "Registry" }),
        el("span", { class: "kv-value kv-value-muted", text: plural(api.projects().length, "project") }),
      ),
    ),
  );

  // ------------------------------------------------------------- lifecycle --

  const unsubscribers = [
    api.onBus("state", (changed) => {
      if ("projectsRoot" in changed) renderProjectsRoot();
      if ("servedProjectIds" in changed || "daemonSessions" in changed) {
        renderServed();
        // Both the plugin's protocol check and the About line read a *served*
        // daemon, so which project is served is exactly what changes them.
        void refreshPluginStatus();
        void refreshDaemonIdentity();
      }
      if ("appearanceTheme" in changed) renderThemes();
    }),
    // The broker can come up or go down without this view touching it: at boot
    // for a folder authorized in an earlier run, or when it loses its port.
    api.onBus("broker", () => renderProjectsRoot()),
    // A daemon coming up or going down is what makes the protocol comparison
    // possible or impossible; neither passes through `state`.
    api.onBus("daemon", () => {
      void refreshPluginStatus();
      void refreshDaemonIdentity();
    }),
  ];

  api.setStatus("Appearance, projects folder, the API key and the plugin install are live.");

  return () => {
    for (const unsubscribe of unsubscribers) unsubscribe();
  };
}

function section(title, sub, ...children) {
  return el(
    "section",
    { class: "section" },
    el(
      "div",
      { class: "section-head" },
      el(
        "div",
        {},
        el("h2", { class: "section-title", text: title }),
        sub ? el("p", { class: "section-sub", text: sub }) : null,
      ),
    ),
    el("div", { class: "section-body" }, ...children),
  );
}

function showNotice(node, tone, message) {
  node.hidden = false;
  node.className = `notice notice-${tone === "ok" ? "accent" : tone}`;
  // `notice-text` keeps the host's line breaks. Several of these messages are
  // instructions — "build it:\n  node plugin/scripts/build.mjs" — and a command
  // reflowed into the middle of a paragraph is a command nobody spots.
  node.replaceChildren(icon("alert", 14), el("span", { class: "notice-text", text: message }));
}

/** "Aug 10, 4:14 AM" from the manifest's ISO `builtAt`; null when absent or unparsable. */
function formatBuilt(iso) {
  if (typeof iso !== "string" || iso === "") return null;
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return null;
  return date.toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
}

/** A plugin artifact's size, at the precision anyone actually reads it at. */
function formatBytes(bytes) {
  if (typeof bytes !== "number" || !Number.isFinite(bytes)) return null;
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * Put text on the clipboard, with a fallback.
 *
 * `navigator.clipboard` needs a secure context, and the macOS webview serves
 * the app from a custom scheme that does not always qualify. The hidden
 * textarea is the path that works there, so both exist rather than a copy
 * button that silently does nothing on one platform.
 */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Fall through.
  }
  try {
    const field = el("textarea", { value: text, "aria-hidden": "true" });
    field.style.cssText = "position:fixed;top:-1000px;opacity:0";
    document.body.append(field);
    field.select();
    const copied = document.execCommand("copy");
    field.remove();
    return copied;
  } catch {
    return false;
  }
}
