// views/projects.js — card list + detail pane (Design 8.2).
//
// Real: adding a project through the native picker, the registry (persisted via
// state_get/state_set), search, filters, selection, removal — and the serve
// toggle, which now starts and stops an actual daemon child through the host.
//
// A failed serve is never swallowed. The project-error card shows the
// classified reason (the `code` the host returned) with the raw diagnostic —
// including the daemon's own stderr tail — behind a collapsible, because the
// useful half of a spawn failure is usually the part nobody wants in a toast.
//
// Still stubbed: the quick actions, which need engine commands rather than the
// daemon lifecycle.

import { el, icon, initials, plural, relativeTime } from "./dom.js";
import { reviewBanner } from "./review.js";

/** How a host error code reads in the project-error card. */
const FAILURE_HEADINGS = {
  unavailable: "No engine binary.",
  daemon: "The daemon refused to start.",
  protocol: "The daemon did not speak the expected protocol.",
  timeout: "The daemon did not report itself in time.",
  io: "Could not run the daemon.",
  invalid_argument: "That request was rejected.",
  no_host: "Serving needs the WSync desktop host.",
  exited: "The daemon exited.",
  killed: "The daemon was terminated.",
  "heartbeat-lost": "The daemon stopped answering heartbeats.",
  // Design 8.4: the broker created the project but its auto-serve did not take.
  // The project is real and on disk; only the daemon is missing.
  "serve-failed": "The project was created, but its daemon would not start.",
};

const FILTERS = [
  { id: "all", label: "All" },
  { id: "serving", label: "Serving" },
  { id: "setup", label: "Needs setup" },
];

export function mountProjects(root, api) {
  let query = "";
  let filter = "all";
  let confirmingRemoval = null;
  let confirmTimer = null;
  /** Projects with a serve/stop in flight — the toggle must not be clickable twice. */
  const busy = new Set();

  // --- static skeleton, built once so the search field keeps focus ---------

  const searchInput = el("input", {
    class: "input",
    type: "search",
    placeholder: "Search projects…",
    "aria-label": "Search projects",
    on: {
      input: (event) => {
        query = event.target.value.trim().toLowerCase();
        renderList();
      },
    },
  });

  const pillBar = el("div", { class: "pills", role: "group", "aria-label": "Filter projects" });
  const addButton = el(
    "button",
    { class: "btn btn-primary", type: "button", on: { click: addProjectFlow } },
    icon("plus"),
    "Add project",
  );

  const toolbar = el(
    "div",
    { class: "toolbar inset-x" },
    el("div", { class: "search" }, el("span", { class: "search-icon" }, icon("search", 13)), searchInput),
    pillBar,
    el("div", { class: "toolbar-spacer" }),
    addButton,
  );

  // `list`, not `listbox`: a card is a composite (it contains its own serve
  // toggle), so option semantics would be a lie to a screen reader.
  const listScroll = el("div", { class: "list-scroll", role: "list", "aria-label": "Projects" });
  const detailPane = el("div", { class: "detail-scroll" });
  const workspace = el(
    "div",
    { class: "workspace" },
    el("div", { class: "workspace-list" }, listScroll),
    el("div", { class: "workspace-detail" }, detailPane),
  );

  root.append(toolbar, workspace);

  // --- rendering -----------------------------------------------------------

  function visibleProjects() {
    return api.projects().filter((project) => {
      if (query) {
        const haystack = `${project.name} ${project.path}`.toLowerCase();
        if (!haystack.includes(query)) return false;
      }
      if (filter === "serving") return api.isProjectServed(project.id);
      if (filter === "setup") return !project.gameId;
      return true;
    });
  }

  function renderPills() {
    const all = api.projects();
    const counts = {
      all: all.length,
      serving: all.filter((project) => api.isProjectServed(project.id)).length,
      setup: all.filter((project) => !project.gameId).length,
    };
    pillBar.replaceChildren(
      ...FILTERS.map((entry) =>
        el(
          "button",
          {
            class: "pill",
            type: "button",
            "aria-pressed": filter === entry.id ? "true" : "false",
            on: {
              click: () => {
                filter = entry.id;
                renderPills();
                renderList();
              },
            },
          },
          entry.label,
          el("span", { class: "pill-count", text: String(counts[entry.id]) }),
        ),
      ),
    );
  }

  /**
   * Design 8.4: a project WSync created for Studio, rather than one the user
   * pointed at. Worth saying on the card — its folder was made by the broker,
   * inside the authorized projects folder, and its source tree starts empty.
   */
  function studioChip(project) {
    if (project.initializedFromStudio !== true) return null;
    return el("span", {
      class: "chip chip-plain",
      text: "From Studio",
      title: "Created from Studio through the project broker",
    });
  }

  function statusChip(project) {
    const failure = api.getDaemonFailure(project.id);
    if (api.isProjectServed(project.id)) {
      const session = api.getDaemonSession(project.id);
      const link = api.getLinkState();
      // The link only follows one project at a time; only that project's chip
      // may claim anything about the socket.
      if (link?.projectId === project.id) {
        if (link.state === api.linkStates.RECONNECTING) {
          return el("span", { class: "chip chip-warn", text: "Reconnecting" });
        }
        if (link.state === api.linkStates.STOPPED) {
          return el("span", { class: "chip chip-danger", text: "Updates stopped" });
        }
      }
      if (session?.alreadyRunning) return el("span", { class: "chip chip-ok", text: "Joined" });
      return el("span", { class: "chip chip-ok", text: "Serving" });
    }
    if (busy.has(project.id)) return el("span", { class: "chip chip-accent", text: "Starting…" });
    if (failure) return el("span", { class: "chip chip-danger", text: "Failed" });
    return el("span", { class: "chip", text: "Idle" });
  }

  function serveToggle(project, { withLabel = false } = {}) {
    const served = api.isProjectServed(project.id);
    const working = busy.has(project.id);
    const input = el("input", {
      type: "checkbox",
      checked: served,
      disabled: working,
      "aria-label": `Serve ${project.name}`,
      on: {
        change: async (event) => {
          const wanted = event.target.checked;
          event.target.disabled = true;
          busy.add(project.id);
          render();
          try {
            if (wanted) await api.serveProject(project.id);
            else await api.stopProject(project.id);
          } finally {
            busy.delete(project.id);
            render();
          }
        },
        // The card is selectable; the toggle is not a way to select it.
        click: (event) => event.stopPropagation(),
      },
    });
    return el(
      "label",
      { class: "toggle", on: { click: (event) => event.stopPropagation() } },
      input,
      el("span", { class: "toggle-track" }),
      withLabel
        ? el("span", {
            class: "toggle-label",
            text: working ? "Working…" : served ? "Serving" : "Not serving",
          })
        : null,
    );
  }

  function projectCard(project) {
    const selected = api.getState().activeProjectId === project.id;
    const select = () => {
      api.setActiveProject(project.id);
      render();
    };
    return el(
      "div",
      {
        class: "project-card",
        role: "listitem",
        tabindex: "0",
        data: { selected: selected ? "true" : "false" },
        "aria-current": selected ? "true" : null,
        on: {
          click: select,
          keydown: (event) => {
            if (event.key === "Enter" || event.key === " ") {
              event.preventDefault();
              select();
            }
          },
        },
      },
      el("span", { class: "project-thumb", text: initials(project.name), "aria-hidden": "true" }),
      el(
        "div",
        { class: "project-main" },
        el("div", { class: "project-name", text: project.name, title: project.name }),
        el(
          "div",
          { class: "project-meta" },
          el("span", { class: "path", text: project.path, title: project.path }),
        ),
      ),
      el(
        "div",
        { class: "project-side" },
        studioChip(project),
        statusChip(project),
        serveToggle(project),
      ),
    );
  }

  function renderList() {
    renderPills();
    const all = api.projects();

    if (all.length === 0) {
      listScroll.replaceChildren(
        el(
          "div",
          { class: "empty" },
          el("div", { class: "empty-icon" }, icon("folder", 17)),
          el("div", { class: "empty-title", text: "No projects yet" }),
          el("p", {
            class: "empty-body",
            text: "Pick a folder that holds a WSync or Rojo project file. WSync only ever learns a path from the native picker.",
          }),
          el(
            "div",
            { class: "empty-actions" },
            el(
              "button",
              { class: "btn btn-primary", type: "button", on: { click: addProjectFlow } },
              icon("plus"),
              "Add your first project",
            ),
          ),
        ),
      );
      return;
    }

    const visible = visibleProjects();
    const cards = visible.map((project) => projectCard(project));
    if (visible.length === 0) {
      cards.push(
        el("div", { class: "stage-empty", text: `No projects match "${searchInput.value.trim()}".` }),
      );
    }
    cards.push(
      el(
        "button",
        { class: "add-tile", type: "button", on: { click: addProjectFlow } },
        el("span", { class: "add-tile-icon" }, icon("plus", 15)),
        el(
          "span",
          {},
          el("span", { class: "add-tile-title", text: "Add project" }),
          el("br"),
          el("span", { class: "add-tile-sub", text: "Choose a folder on this computer" }),
        ),
      ),
    );
    listScroll.replaceChildren(...cards);
  }

  function renderDetail() {
    const project = api.getProject(api.getState().activeProjectId);
    if (!project) {
      detailPane.replaceChildren(
        el(
          "div",
          { class: "empty" },
          el("div", { class: "empty-icon" }, icon("arrowLeft", 17)),
          el("div", { class: "empty-title", text: "Nothing selected" }),
          el("p", { class: "empty-body", text: "Choose a project to see its details." }),
        ),
      );
      return;
    }

    const failure = api.getDaemonFailure(project.id);
    const removing = confirmingRemoval === project.id;
    // Design 7.0: the disk review belongs to a project, so it is shown on one —
    // and only on the project it is about. Non-blocking by construction: it is
    // a strip above the details, not an overlay.
    const pendingReview = api.getPendingReview?.();
    const banner =
      pendingReview?.projectId === project.id
        ? reviewBanner(api, pendingReview, { style: "margin:0 0 14px" })
        : null;

    detailPane.replaceChildren(
      // Spread rather than passed: `replaceChildren` is native and would turn a
      // null into the text "null".
      ...(banner ? [banner] : []),
      el(
        "div",
        { class: "detail-head" },
        el("span", { class: "detail-thumb", text: initials(project.name), "aria-hidden": "true" }),
        el(
          "div",
          { class: "detail-heading" },
          el("h2", { class: "detail-name", text: project.name }),
          el(
            "div",
            { class: "detail-chips" },
            statusChip(project),
            studioChip(project),
            // Design 8.2's sync-scope chip, saying what Design 7.0 made true:
            // code scope is the projection unless a project file explicitly
            // opts into `"scope": "full"`, which is a file the app does not
            // read. "Full DataModel" here was a claim about every project, and
            // it is now the wrong one for all but a hand-edited few.
            el("span", { class: "chip chip-plain", text: "Code scope" }),
            project.gameId
              ? el("span", { class: "chip chip-plain", text: `Game ${project.gameId}` })
              : el("span", { class: "chip chip-warn", text: "No linked game" }),
            project.wallyEnabled ? el("span", { class: "chip chip-plain", text: "Wally" }) : null,
          ),
        ),
      ),

      el(
        "div",
        { class: "kv" },
        el("span", { class: "kv-key", text: "Path" }),
        el("span", { class: "kv-value path", text: project.path, title: project.path }),
        el("span", { class: "kv-key", text: "Added" }),
        el("span", { class: "kv-value kv-value-muted", text: relativeTime(project.addedAt) }),
        el("span", { class: "kv-key", text: "Game ID" }),
        el("span", { class: "kv-value kv-value-muted", text: project.gameId ?? "Not linked" }),
        el("span", { class: "kv-key", text: "Group ID" }),
        el("span", { class: "kv-value kv-value-muted", text: project.groupId ?? "Not linked" }),
        el("span", { class: "kv-key", text: "Place IDs" }),
        el("span", {
          class: "kv-value kv-value-muted",
          text: project.placeIds?.length ? project.placeIds.join(", ") : "None",
        }),
      ),

      el(
        "div",
        { class: "detail-group" },
        el("div", { class: "detail-group-title", text: "Serving" }),
        el(
          "div",
          { class: "row" },
          el(
            "div",
            { class: "row-copy" },
            el("div", { class: "row-title", text: "Run a daemon for this project" }),
            el("div", {
              class: "row-sub",
              text: "One daemon per project, loopback only, port 7978–7990.",
            }),
          ),
          el("div", { class: "row-actions" }, serveToggle(project, { withLabel: true })),
        ),
        sessionFacts(project),
        errorCard(failure),
      ),

      el(
        "div",
        { class: "detail-group" },
        el("div", { class: "detail-group-title", text: "Quick actions" }),
        el(
          "div",
          { class: "detail-actions" },
          quickAction("Build", "Builds an .rbxl from the live tree."),
          quickAction("Sourcemap", "Writes sourcemap.json for editors."),
          quickAction("Open folder", "Reveals the project in your file manager."),
        ),
        el("p", {
          class: "field-hint",
          style: "margin-top:9px",
          text: "Quick actions run engine commands over the daemon; they land with the command surface.",
        }),
      ),

      el(
        "div",
        { class: "detail-group" },
        el("div", { class: "detail-group-title", text: "Remove" }),
        el(
          "div",
          { class: "row" },
          el(
            "div",
            { class: "row-copy" },
            el("div", { class: "row-title", text: "Remove from WSync" }),
            el("div", { class: "row-sub", text: "Forgets the project here. Nothing on disk is touched." }),
          ),
          el(
            "div",
            { class: "row-actions" },
            el(
              "button",
              {
                class: "btn btn-danger",
                type: "button",
                on: { click: () => onRemoveClick(project) },
              },
              icon("trash", 13),
              removing ? "Click again to confirm" : "Remove",
            ),
          ),
        ),
      ),
    );
  }

  /** Port, pid and boot id of the live session, plus who owns the process. */
  function sessionFacts(project) {
    const session = api.getDaemonSession(project.id);
    if (!session?.ok) return null;
    const link = api.getLinkState();
    const mine = link?.projectId === project.id;
    return el(
      "div",
      { class: "kv kv-tight", style: "margin-top:10px" },
      el("span", { class: "kv-key", text: "Address" }),
      el("span", { class: "kv-value path", text: session.base }),
      el("span", { class: "kv-key", text: "Process" }),
      el("span", {
        class: "kv-value kv-value-muted",
        text: session.managed === false ? `pid ${session.pid} (owned elsewhere)` : `pid ${session.pid}`,
      }),
      el("span", { class: "kv-key", text: "Boot ID" }),
      el("span", { class: "kv-value kv-value-muted path", text: session.bootId }),
      el("span", { class: "kv-key", text: "Updates" }),
      el("span", {
        class: "kv-value kv-value-muted",
        text: mine ? (link.detail ?? link.state) : "Following another project",
      }),
    );
  }

  /**
   * Design 8.2's project-error card. The one-line classification is what the
   * user reads; the raw diagnostic — argv failures, the daemon's refusal, its
   * stderr tail — is one click away and never truncated into uselessness.
   */
  function errorCard(failure) {
    if (!failure) return null;
    const heading = FAILURE_HEADINGS[failure.code] ?? "Serve failed.";
    const [summary, ...rest] = String(failure.message ?? "").split("\n");
    const diagnostic = rest.join("\n").trim();

    const details = el("details", { class: "diagnostic" });
    details.append(
      el("summary", { text: "Raw diagnostic" }),
      el("pre", {
        class: "diagnostic-body",
        text: diagnostic
          ? `${failure.code}: ${summary}\n${diagnostic}`
          : `${failure.code}: ${failure.message}`,
      }),
    );

    const body = el(
      "div",
      { class: "notice-body" },
      el("div", {}, el("strong", { text: `${heading} ` }), summary || failure.code),
      details,
      failure.at ? el("div", { class: "notice-meta", text: `Reported ${relativeTime(failure.at)}` }) : null,
    );

    return el(
      "div",
      { class: "notice notice-danger", style: "margin-top:10px" },
      icon("alert", 14),
      body,
    );
  }

  function quickAction(label, hint) {
    return el(
      "button",
      {
        class: "btn",
        type: "button",
        disabled: true,
        title: `${hint} (wave 2)`,
        "aria-disabled": "true",
      },
      label,
    );
  }

  function render() {
    renderList();
    renderDetail();
  }

  // --- actions -------------------------------------------------------------

  async function addProjectFlow() {
    try {
      const picked = await api.host.pickProjectFolder();
      const { project, added } = api.addProject({ name: picked.name, path: picked.path });
      if (!added) {
        api.setActiveProject(project.id);
        api.toast("Already added", { kind: "warn", body: picked.path });
      } else {
        api.toast("Project added", { kind: "ok", body: picked.path });
      }
      render();
    } catch (error) {
      if (error?.isCancelled) return;
      if (error?.isHostless) {
        api.toast("Preview mode", {
          kind: "warn",
          body: "Choosing a folder needs the WSync desktop host.",
        });
        return;
      }
      api.toast("Could not add that folder", { kind: "err", body: error?.message ?? String(error) });
    }
  }

  function onRemoveClick(project) {
    if (confirmingRemoval !== project.id) {
      confirmingRemoval = project.id;
      clearTimeout(confirmTimer);
      confirmTimer = setTimeout(() => {
        confirmingRemoval = null;
        renderDetail();
      }, 3500);
      renderDetail();
      return;
    }
    clearTimeout(confirmTimer);
    confirmingRemoval = null;
    api.removeProject(project.id);
    api.toast("Project removed", { body: project.name });
    render();
  }

  // --- lifecycle -----------------------------------------------------------

  const unsubscribers = [
    api.onBus("state", (changed) => {
      if (
        "projects" in changed ||
        "servedProjectIds" in changed ||
        "activeProjectId" in changed ||
        "daemonSessions" in changed
      ) {
        render();
      }
    }),
    api.onBus("daemon", () => render()),
    // Reconnects and non-retryable shutdowns change what a card may claim.
    api.onBus("link", () => render()),
    // Design 8.4: a project the broker just created. The store has already been
    // merged by the time this lands, so the card appears without a reload — and
    // the filter pills recount, which the state subscription alone would also
    // do but only if `projects` happened to be in the same patch.
    api.onBus("project-init", () => render()),
    // Design 7.0: the review banner lives on the project card's detail pane, so
    // it has to redraw when a review arrives, is pushed out, or is dismissed.
    api.onBus("review", () => render()),
  ];

  render();
  api.setStatus(
    api.projects().length ? plural(api.projects().length, "project") : "No projects yet",
  );

  return () => {
    clearTimeout(confirmTimer);
    for (const unsubscribe of unsubscribers) unsubscribe();
  };
}
