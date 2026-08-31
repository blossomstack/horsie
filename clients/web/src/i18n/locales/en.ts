/**
 * The source catalogue. Every other language is typed `typeof en`, so a key
 * added here is a compile error in each of them until it is translated — the
 * completeness check is the type checker, not a script someone remembers to
 * run.
 *
 * Keys are grouped by the surface that draws them; `common` is for words that
 * genuinely repeat (Save, Cancel, a row's Delete) rather than a dumping
 * ground. Values may interpolate with `{{name}}`.
 */
const en = {
  common: {
    save: "Save",
    saving: "Saving…",
    saved: "Saved",
    cancel: "Cancel",
    close: "Close",
    delete: "Delete",
    remove: "Remove",
    edit: "Edit",
    add: "Add",
    create: "Create",
    back: "Back",
    retry: "Retry",
    copy: "Copy",
    copied: "Copied",
    loading: "Loading…",
    none: "None",
    default: "Default",
    optional: "Optional",
    enabled: "Enabled",
    disabled: "Disabled",
    unknown: "Unknown",
    failed: "Failed",
    running: "Running",
    yes: "Yes",
    no: "No",
    search: "Search",
    dismiss: "Dismiss",
    return: "Return",
    open: "Open",
    run: "Run",
    more: "More",
    less: "Less",
    writeFailed: "The write failed.",
    deleteNamed: "Delete {{name}}",
    unreachableShort: "Can\u2019t reach the server.",
  },

  time: {
    justNow: "just now",
    inAMoment: "in a moment",
    ago: "{{value}} ago",
    in: "in {{value}}",
    millisecondsShort: "{{value}}ms",
    secondsShort: "{{value}}s",
    minutesShort: "{{value}}m",
    hoursShort: "{{value}}h",
    daysShort: "{{value}}d",
    hoursMinutesShort: "{{hours}}h {{minutes}}m",
    minutesSecondsShort: "{{minutes}}m {{seconds}}s",
  },

  format: {
    thousands: "{{value}}k",
    millions: "{{value}}M",
  },

  status: {
    provisioning: {
      label: "Provisioning",
      hint: "Building this session's runtime — anything you send runs as soon as it is up.",
    },
    idle: { label: "Idle", hint: "Ready for your next message." },
    running: {
      label: "Running",
      hint: "The agent is working — anything you send is answered next turn.",
    },
    awaitingInput: {
      label: "Awaiting input",
      hint: "The agent asked you a question.",
    },
    finished: {
      label: "Finished",
      hint: "This run completed. Retry a step to take it further.",
    },
    failed: {
      label: "Failed",
      hint: "The last turn failed — send a message to try again.",
    },
    unrecoverable: {
      label: "Unrecoverable",
      hint: "This session's runtime is gone for good. Start a new session.",
    },
  },

  progression: {
    startingRuntime: "Starting runtime…",
    runtimeFailed: "Runtime failed",
    scanningWorkspace: "Scanning workspace…",
    connectingTools: "Connecting tools…",
  },

  session: {
    untitled: "New session",
    noSuch: "No such session",
    loadFailed: "Could not load this session",
    sessionId: "Session id",
    goneHint:
      "It was deleted, or it never existed. Nothing you type here can reach it.",
    readFailed: "The read failed.",
    yourSessionsList: "<lnk>Your sessions</lnk> lists the ones that are there.",
    thisRun: "this run",
    confirmDeleteRun: "Delete \u201c{{name}}\u201d? This cannot be undone.",
    confirmDelete: "Delete this session? This cannot be undone.",
    view: "View",
    reconnecting: "Reconnecting",
    reconnectingHint:
      "Lost the live feed. The run continues on the server; this reconnects and replays anything missed.",
    loadingTranscript: "Loading transcript",
    loadingEarlier: "Loading earlier messages",
    scrollUp: "Scroll up for earlier messages",
    terminal: "This session can no longer run: {{reason}}",
    workflowStepHint:
      "This is a workflow step. It works from its definition, not from messages.",
  },

  ui: {
    showThinking: {
      label: "Show thinking",
      description: "Reveal the model's reasoning steps in the transcript.",
    },
  },

  nav: {
    inbox: "Inbox",
    agents: "Agents",
    environments: "Environments",
    routines: "Routines",
    workflows: "Workflows",
    settings: "Settings",
    admin: "Admin",
  },

  rail: {
    offline: "Offline",
    sessions: "Sessions",
    showSessions: "Show sessions",
    filterByTag: "Filter by tag",
    newSession: "Start a new session",
    filterPlaceholder: "Filter sessions…",
    filterSessions: "Filter sessions",
    unreachable:
      "Can\u2019t reach the server. Check that horsie-server is running, then reload.",
    empty: "No sessions yet. Press <key>+</key> to start one.",
    noTextMatches: "No session matches \u201c{{query}}\u201d.",
    noTagMatches: "No session matches these tags.",
  },

  statusBadge: {
    title: "{{label}} — {{hint}}",
    ariaLabel: "Status: {{label}}",
  },

  themeToggle: {
    toLight: "Switch to light",
    toDark: "Switch to dark",
    ariaLabel: "Toggle light and dark",
  },

  confirm: {
    ariaLabel: "Confirm",
  },

  mutationErrors: {
    failed: "Failed",
  },

  readError: {
    body: "Couldn\u2019t load {{what}}. {{detail}}",
    unreachable:
      "The horsie server is unreachable \u2014 check that it is running, then reload.",
    reload: "Reload to try again.",
  },

  compaction: {
    compacted: "Compacted",
    compactedByHand: "Compacted by hand",
    entries_one: "{{count}} entry",
    entries_other: "{{count}} entries",
    tokensFreed: "{{value}} tokens freed",
    hideDetail: "Hide what was carried across",
    showDetail: "Show the summary and the state carried across",
    summaryHeading: "Summary of earlier work",
    carriedHeading: "Carried across exactly",
    nothingToCompact: "Nothing to compact",
    tokensKept: "{{used}} of {{retain}} tokens kept",
    noWindow:
      "This model declares no context window, so there is no budget to compact against.",
    nothingToFold:
      "This session is about {{used}} tokens and a compaction keeps the most recent {{retain}} verbatim \u2014 so there is nothing before that to fold. Compacting anyway would trade real messages for a summary to buy room that is not scarce.",
  },

  artifact: {
    untitled: "Untitled file",
    openFull: "Open at full size",
    download: "Download",
    close: "Close",
  },

  composer: {
    ariaLabel: "Message the agent",
    idlePlaceholder: "Message the agent\u2026",
    answerPlaceholder: "Answer the agent\u2026",
    queuePlaceholder: "Queue a message for the next turn\u2026",
    stop: "Stop this turn",
    stopTitle: "Stop this turn \u2014 queued messages are kept",
    send: "Send message",
    sendTitle: "Send \u2014 Enter sends, Shift+Enter starts a new line",
    attach: "Attach a file",
    attachTitle: "Attach an image or a PDF",
    attachUnavailable: "Attachments are not available here.",
    attachRefused: "Only PNG, JPEG, GIF, WebP and PDF files can be attached.",
    attachPending: "Wait for the attachments to finish uploading",
    uploading: "Uploading",
    uploadFailed: "Upload failed",
    removeAttachment: "Remove this attachment",
    pastedName: "Pasted file",
  },

  turnActions: {
    copyMarkdown: "Copy as markdown",
    copyPlain: "Copy as plain text",
    markdownCopied: "Markdown copied",
    textCopied: "Text copied",
  },

  spine: {
    jumpStart: "Jump to the start of the session",
    jumpEnd: "Jump to the end of the session",
    scroll: "Scroll the transcript",
    sessionEnded: "Session {{index}} ended here",
    sessionEndedSummarised_one:
      "Session {{index}} ended here \u2014 {{count}} entry summarised",
    sessionEndedSummarised_other:
      "Session {{index}} ended here \u2014 {{count}} entries summarised",
    jumpToCompaction: "Jump to compaction {{index}} of {{total}}",
  },

  agentGraph: {
    openRun: "Open the {{label}} run",
    openTranscript: "Open {{label}}'s transcript",
    showSpawned: "Show what {{label}} spawned \u2014 {{count}} hidden",
    hideSpawned: "Hide what {{label}} spawned",
    empty:
      "No agents have been recorded for this session yet. The graph draws itself as they start.",
    ariaLabel: "Agent graph",
    nodeTitle: "{{label}} \u2014 {{kind}}, {{detail}}",
    currentRun: " \u00b7 the run you are reading",
  },

  entryMenu: {
    ariaLabel: "Commands, skills and agents",
  },

  timeline: {
    thinkingBlocks_one: "Thinking \u00b7 {{count}} block",
    thinkingBlocks_other: "Thinking \u00b7 {{count}} blocks",
    thisSession: "this session",
    thisAgent: "this agent",
    sessionCompacted: "Session compacted",
    empty:
      "Nothing has happened in this session yet. The timeline draws itself as the agent works.",
    unplaced:
      "not on the timeline \u2014 nothing was recorded about when these ran",
  },

  subagent: {
    label: "subagent",
  },

  askUser: {
    notAnswered: "Not answered \u2014 {{answer}}",
    orOwnWords: "Or answer in your own words\u2026",
    yourAnswer: "Your answer\u2026",
    sendAnswer: "Send answer",
    sendAllAnswers: "Send all answers",
    oneOfMany:
      "One of {{total}} questions \u2014 all of them are sent together.",
  },

  tagFilter: {
    clear: "Clear",
  },

  taskList: {
    legend: "Plan",
    progress: "{{done}}/{{total}} done",
    hide: "Hide the plan",
    show: "Show the plan",
    hideWithCount: "Hide the plan \u2014 {{done}}/{{total}} done",
    showWithCount: "Show the plan \u2014 {{done}}/{{total}} done",
    toggle: "Toggle the agent's plan",
    toggleWithCount: "Toggle the agent's plan \u2014 {{done}} of {{total}} done",
    empty:
      "No plan yet. The agent writes one here when a task is big enough to need steps.",
  },

  thinking: {
    label: "Thought for a moment",
  },

  projects: {
    newProject: "New project\u2026",
  },

  projectsPage: {
    what: "projects",
    readErrorDesc: "This account's projects.",
    desc: "One per body of work. Nothing is shared between them.",
    sectionDesc:
      "Everything else on this rail belongs to one project: its models, runtimes, skills, memory spaces, integrations, agents and sessions. A new project starts empty, credentials included.",
    empty: "No projects yet.",
    defaultHint: "Always present, and cannot be deleted",
    cannotDelete: "The default project cannot be deleted",
    saveName: "Save name",
    newProject: "New project",
    newProjectDesc:
      "It starts empty \u2014 add its models and runtimes once it exists.",
    namePlaceholder: "What is this project for?",
    confirmDelete:
      "Delete project \u201c{{name}}\u201d? Its sessions, agents, settings and memories go with it, and its runtimes are destroyed. This cannot be undone.",
  },

  toolCall: {
    input: "Input",
    output: "Output",
    error: "Error",
    returnedNothing: "Returned nothing",
    pluginHooks: "Plugin hooks",
    blockedBy: "Blocked by {{plugin}}",
    artifacts: "Files",
  },

  transcript: {
    working: "The agent is working",
    queued: "Unsent \u2014 goes in with the next turn",
  },

  workGroup: {
    working: "Working",
    thoughtOnly: "Thought for a moment",
    plain: {
      tools_one: "Ran {{tools}} tool",
      tools_other: "Ran {{tools}} tools",
      subagents_one: "{{subagents}} subagent finished",
      subagents_other: "{{subagents}} subagents finished",
      both: "Ran {{tools}} tools and {{subagents}} subagents finished",
    },
    thought: {
      tools_one: "Thought and ran {{tools}} tool",
      tools_other: "Thought and ran {{tools}} tools",
      subagents_one: "Thought and {{subagents}} subagent finished",
      subagents_other: "Thought and {{subagents}} subagents finished",
      both: "Thought, ran {{tools}} tools and {{subagents}} subagents finished",
    },
  },

  workflowGraph: {
    empty: "Add a step to see the graph.",
    ariaLabel: "Workflow graph",
    start: "start",
  },

  sessionRow: {
    renameSession: "Rename session",
    actions: "Session actions",
    newTag: "New tag",
    newTagPlaceholder: "New tag\u2026",
    rename: "Rename",
  },

  usage: {
    input: "Input",
    inputHint:
      "Full prompt tokens: system prompt, tool definitions, and the session history so far. Cache reads/writes below are included in this total, not additional.",
    output: "Output",
    outputHint: "Tokens the model generated back.",
    cacheRead: "Cache read",
    cacheReadHint:
      "Served from the provider's prompt cache at a steep discount, instead of being reprocessed at full price.",
    cacheWrite: "Cache write",
    cacheWriteHint:
      "Written to the provider's prompt cache this turn at a premium \u2014 pays off as cache reads on later turns that reuse it.",
  },

  gauge: {
    buttonLabel: "Context {{percent}}% full \u2014 {{word}}. Open token usage.",
    buttonLabelUnknown: "Open token usage",
    buttonTitle:
      "Context {{percent}}% full \u2014 {{word}}. {{used}} of {{window}}. Click for the token breakdown.",
    buttonTitleUnknown:
      "{{spent}} tokens spent. Context window unknown for this model. Click for the token breakdown.",
    nearlyFull: "Nearly full",
    filling: "Filling",
    roomToSpare: "Room to spare",
    contextWindow: "Context window",
    windowHint:
      "Tokens currently loaded in the main agent's context, out of its context window. Cache status doesn't shrink this \u2014 it only affects price and speed.",
    used: "Context window used",
    compactsAt: "Compacts automatically around {{percent}}% full",
    thisTurn: "This turn",
    sessionTotal: "Session total",
    sessionTotalHint:
      "Everything this session has spent, across every agent it hosts. This is cost, not context fullness \u2014 the dial above is context.",
  },

  entryPanel: {
    legend: "Entry",
    close: "Hide the entry panel",
    timing: "Timing",
    at: "At",
    took: "Took",
    tookHint: "How long the provider call that produced this message took.",
    message: "Message",
    noText: "This entry carries no text of its own \u2014 it is the work it set off.",
    thinking: "Thinking",
    toolCalls: "Tool calls",
    running: "running",
    readInTranscript: "Read in transcript",
  },

  agentPanel: {
    agent: "Agent",
    run: "Run",
    transcript: "Transcript",
    close: "Hide the agent panel",
    contextUsed: "Context used",
    timing: "Timing",
    branched: "Branched",
    opened: "Opened",
    spawned: "Spawned",
    lastActivity: "Last activity",
    ended: "Ended",
    runningFor: "Running for",
    runningForHint: "Measured against now: this agent has not stopped.",
    took: "Took",
    tookHint: "From when it began to when it reached this result.",
    context: "Context",
    inContext: "In context",
    inContextHint:
      "No window is configured for this agent's model, so there is no fraction to draw.",
    asOfLastTurn: "As of the end of this agent's last turn.",
    tokens: "Tokens",
    inputHint:
      "Full prompt tokens across this agent's turns. Cache reads and writes are included in this total, not additional.",
    outputHint: "Tokens this agent generated back.",
    cacheReadHint: "Served from the provider's prompt cache at a discount.",
    cacheWriteHint: "Written to the provider's prompt cache at a premium.",
    withSubtree: "With subtree",
    withSubtreeHint:
      "This agent plus everything below it: the subagents it spawned, the sub sessions branched from it, and the steps of any workflow it invoked.",
    brief: "Brief",
    task: "Task",
    result: "Result",
    deleteAgent: "Delete this agent",
    deleteSubSession: "Delete this sub session",
    deleteSubagent: "Delete this subagent run and everything below it",
  },

  hook: {
    allowed: "allowed",
    ran: "ran",
    noReason: "no reason given",
    couldNotRun: "could not run \u2014 {{reason}}",
    rewroteInput: "rewrote the input",
    rewroteOutput: "rewrote the output",
    stoppedHorsie: "stopped horsie \u2014 {{why}} ({{outcome}})",
    deniedCall: "denied the call",
    askedApproval: "asked for approval \u2014 allowed",
    addedResultContext: "added context to the result",
    objectedAlreadyRan: "objected \u2014 the call had already run",
    addedContext: "added context",
    objected: "objected",
    addedSessionContext: "added session context",
    addedPromptContext: "added context to the prompt",
    rejectedPrompt: "rejected the prompt",
    leftNote: "left a note for the next turn",
    keptTurnGoing: "kept the turn going \u2014 {{reason}}",
    hitContinuationLimit: "hit the continuation limit \u2014 {{reason}}",
    stoppedCompaction: "stopped the compaction \u2014 {{reason}}",
  },

  agentKind: {
    main: "main session",
    subagent: "subagent",
    step: "workflow step",
    sub_session: "sub session",
    run: "workflow run",
    mainAgent: "main agent",
  },

  schedule: {
    manually: "manually",
    every: "every {{interval}}",
    once: "once on {{when}}",
    daily: "daily at {{time}} ({{timezone}})",
    weekly: "every {{days}} at {{time}} ({{timezone}})",
    monthly: "monthly on the {{day}} at {{time}} ({{timezone}})",
    yearly: "yearly on {{month}} {{day}} at {{time}} ({{timezone}})",
    ordinalOne: "{{n}}st",
    ordinalTwo: "{{n}}nd",
    ordinalFew: "{{n}}rd",
    ordinalOther: "{{n}}th",
  },

  newSession: {
    workflowRun: "Workflow run",
    stepCount_one: "{{count}} step",
    stepCount_other: "{{count}} steps",
    startsAt: "starts at",
    loadFailed:
      "Couldn\u2019t load this server\u2019s models and runtimes. Reload once the server is reachable.",
    needModel: "Select a model or agent to start.",
    needEnvironment: "Select an environment to start.",
    needGithub: "Connect GitHub to use these repos.",
    attachmentsUnsupported:
      "Attachments can\u2019t be sent with a workflow run or an agent invocation. Remove them, or start a plain session.",
  },

  tools: {
    read: "read",
    write: "write",
    selectAll: "Select all",
    selectAllIn: "Select all {{group}} tools",
    filterAll: "All",
    filterRead: "Read",
    filterWrite: "Write",
  },

  channel: {
    tools: "Tools",
    toolCatalogue: "the tool catalogue",
    environment: "Environment",
    workflow: "Workflow",
    model: "Model",
    models: "models",
    skills: "Skills",
    skillBundles: "skill bundles",
    installSkills: "Install skill bundles in Settings",
    mcp: "MCP",
    mcpServers: "MCP servers",
    mcpServers2: "MCP servers",
    addMcp: "Add MCP servers in Settings",
    memory: "Memory",
    memorySpaces: "memory spaces",
    createMemory: "Create a memory space first",
    thinking: "Thinking",
    select: "Select",
    selectedCount: "{{count}} selected",
    noModels: "No models configured \u2014 add one in Settings",
    defaultEffort: "default ({{effort}})",
    defaultLower: "default",
    defaultToolSet:
      "Every built-in tool except the control plane \u2014 this server's default set.",
    modelMissing: "{{model}} \u2014 missing",
    modelGoneHint:
      "This model is no longer configured, so the next turn in this session will fail. Restore the alias in Settings \u2192 Models, or start a new session.",
  },

  modelChannel: {
    models: "Models",
    agents: "Agents",
    /** The heading over what a created session's preset resolved to. */
    resolved: "Runs with",
    presetGone: "{{agent}} \u2014 deleted",
    presetGoneHint:
      "This preset no longer exists. The session keeps the settings it was created with \u2014 they are frozen below \u2014 but the preset itself cannot be opened or reused.",
  },

  workflowChannel: {
    oneAgent: "one agent",
    define: "Define a workflow to run one",
  },

  environment: {
    predefined: "Predefined",
    runtimes: "Runtimes",
    runtimesLower: "runtimes",
    repos: "Repos",
    reposLower: "repos",
    repoCount_one: "{{count}} repo",
    repoCount_other: "{{count}} repos",
    defaultVendor: "default",
    noRuntime:
      "No runtime is connected, so a session can\u2019t run a turn yet. Run <cmd>horsie connect</cmd> on the machine holding your code.",
    connectGithub: "Connect GitHub in Settings to pick repos",
    connectGithub2: "Connect GitHub",
    noRepos: "No repos visible to the app installation.",
  },

  notFound: {
    title: "Not found",
    desc: "No page is served at this address.",
    requestedPath: "Requested path",
    help: "Check the address for a typo, or pick up where you left off from <lnk>your sessions</lnk>. The rail on the left reaches everything else.",
  },

  layout: {
    skipToContent: "Skip to content",
    closeSessions: "Close sessions",
  },

  login: {
    signIn: "Sign in",
    signingIn: "Signing in\u2026",
    password: "Password",
    passwordHint:
      "This server requires a password. On first boot horsie writes a generated one to <file>initial-admin-password</file> in its state directory.",
    failed:
      "Could not sign in. Check the server is still running, then try again.",
  },

  settingsHeader: {
    saving: "Saving",
    unsaved: "Unsaved",
    discard: "Discard",
  },

  agents: {
    new: "New agent",
    loading: "Loading agents",
    rosterTitle: "Agent roster",
    rosterBlurb:
      "An agent is a saved session setup \u2014 runtime, model, repos, skills, memory \u2014 so a run you repeat does not have to be reassembled each time. Press <key>New agent</key> to define one, then invoke it from any machine:",
    skillCount_one: "{{count}} skill",
    skillCount_other: "{{count}} skills",
    memoryCount_one: "{{count}} memory",
    memoryCount_other: "{{count}} memory",
    mcpCount: "{{count}} MCP",
    confirmDelete: "Delete agent '{{name}}'?",
  },

  environments: {
    confirmDelete: "Delete environment '{{name}}'?",
    new: "New environment",
    loading: "Loading environments",
    rosterTitle: "Environment roster",
    rosterBlurb:
      "An environment is a saved runtime + repos bundle \u2014 where the work runs and what is checked out there. Press <key>New environment</key> to define one.",
  },

  routines: {
    confirmDelete: "Delete routine '{{name}}' and every session it created?",
    new: "New routine",
    noSuch: "No such routine: {{name}}.",
    runFailed: "Failed to run.",
    paused: "paused",
    starting: "Starting\u2026",
    runNow: "Run now",
    agent: "Agent",
    noRuntime: "No runtime",
    runs: "Runs",
    runsRead: "this routine's runs",
    next: "Next",
    notScheduled: "not scheduled",
    prompt: "Prompt",
    lastTriggerFailed: "Last trigger failed: {{error}}",
    noRuns:
      "No runs yet. Runs appear here rather than in the rail, and each works from the prompt alone \u2014 it has no way to ask you a question.",
    rosterTitle: "Routine roster",
    rosterBlurb:
      "A routine runs an agent against a fixed prompt \u2014 on a timer, from the API, or whenever you press run. Its sessions live on its own page rather than in the rail. Press <key>New routine</key> to define one.",
  },

  workflows: {
    rowMeta_one: "{{count}} step \u00b7 starts at {{start}}",
    rowMeta_other: "{{count}} steps \u00b7 starts at {{start}}",
    confirmDelete:
      'Delete workflow "{{name}}"? Its runs stay in the session rail.',
    new: "New workflow",
    noSuch: "No such workflow.",
    graph: "Graph",
    graphBlurb:
      "Every step shares one runtime and one workspace. <step>{{start}}</step> is handed the input the run starts with.",
    runsRead: "this workflow's runs",
    noRuns: "No runs yet.",
    rosterTitle: "Workflow roster",
    rosterBlurb:
      "A workflow runs several agents in order, each one deciding where the next goes. Every step shares one workspace, so what one writes the next one reads. Runs appear in the rail alongside your sessions. Press <key>New workflow</key> to define one.",
  },

  inbox: {
    title: "Inbox",
    badgeLabel: "{{unread}} unread, {{openAsks}} waiting on an answer",
    filterAll: "All",
    filterUnread: "Unread",
    filterOpen: "Needs answer",
    empty:
      "Nothing here. Agents write to this inbox when they have something to tell you, or a question they cannot get past.",
    noneInView: "Nothing in this view.",
    kindNotice: "Notice",
    kindAsk: "Question",
    unread: "Unread",
    waiting: "Waiting on you",
    openSession: "Open session",
    pickOne: "Pick a message to read it.",
    select: "Select \u201c{{title}}\u201d",
    selectAll: "Select every message",
    deleteSelected_one: "Delete {{count}} message",
    deleteSelected_other: "Delete {{count}} messages",
    confirmDelete_one: "Delete this message?",
    // The whole selection is one question, so there is no "of them" to speak of
    // — the two-sentence form reads as a mismatch at a count of one.
    confirmDeleteOnlyAsk:
      "Delete this question? The agent is still parked on it. Deleting it declines the question: the agent is told nobody will answer, and carries on without one.",
    confirmDelete_other: "Delete these {{count}} messages?",
    declineWarning_one:
      "{{count}} of them is a question an agent is still parked on. Deleting it declines the question: the agent is told nobody will answer, and carries on without one.",
    declineWarning_other:
      "{{count}} of them are questions agents are still parked on. Deleting them declines those questions: each agent is told nobody will answer, and carries on without one.",
    answered: "You answered this.",
    declined:
      "You declined this. The agent was told nobody would answer and carried on without one.",
    closed: "Never answered \u2014 something later in the session moved past it.",
    replyPlaceholder: "Reply to this agent\u2026",
    send: "Send",
  },

  settingsNav: {
    projects: "Projects",
    models: "Models",
    runtimes: "Runtimes",
    skills: "Skills",
    memory: "Memory",
    integrations: "Integrations",
    appearance: "Appearance",
    account: "Account",
  },

  adminNav: {
    modelCards: "Model cards",
    githubApp: "GitHub App",
  },

  device: {
    title: "Authorize a command-line login",
    desc: "Check that this code matches the one your terminal printed. Approving grants that machine access to this server as you.",
    approved:
      "Approved. Your terminal should continue in a few seconds \u2014 you can close this page.",
    denied: "Denied. That login attempt was refused.",
    codePlaceholder: "XXXX-XXXX",
    approve: "Approve",
    deny: "Deny",
  },

  chatgpt: {
    signedIn: "Signed in",
    signOut: "Sign out",
    notSignedIn: "Not signed in",
    signIn: "Sign in with ChatGPT",
    starting: "Starting\u2026",
    openAndEnter: "Open <here>{{url}}</here> and enter this code:",
    waiting:
      "Waiting for approval\u2026 you can do this on any device. Usage draws on this ChatGPT plan's Codex limits.",
  },

  skills: {
    hooks: "hooks",
    defaultForNew: "Default for new sessions",
    update: "Update",
    deleteBundle: "Delete bundle",
    confirmDeleteBundle: 'Delete skill bundle "{{name}}"?',
    removeMarketplace: "Remove marketplace",
    confirmRemoveMarketplace:
      'Remove marketplace "{{name}}"? Bundles installed from it stay installed.',
    filterPlugins: "Filter plugins\u2026",
    filterPluginsLabel: "Filter plugins",
  },

  authored: {
    historyOf: "History of {{name}}",
    confirmDeleteSkill:
      'Delete skill "{{name}}"? Its history is kept, so it can be restored.',
    confirmDeletePlugin:
      'Delete "{{name}}"? This removes every skill in it and its entry in the bundle library.',
    title: "Authored here",
    desc: "Plugins whose skills live in this server's database. Agents write them through the authoring tools; you can read, roll back and remove them here.",
    loadingHistory: "Loading history\u2026",
    historyFailed: "Could not read this skill's history.",
    deleted: "deleted",
    restore: "restore",
    newPlugin: "New plugin",
    newPluginPlaceholder: "field-notes",
    empty:
      "Nothing authored yet. A session with the authoring tools selected can write skills here.",
  },

  run: {
    interrupt: "Interrupt",
    retry: "Retry",
    confirmRetry:
      "Retry {{step}}? It runs again against the workspace as the previous attempt left it.",
    steps: "Steps",
    stepRunning: "A step is currently running.",
    retryHint: "Re-run this step. The workspace is not rolled back.",
    /** The view switch's two disabled keys, on a run with no step open. */
    noRunTranscript: "A run has no transcript of its own \u2014 open a step to read one",
    runHint: "The graph is this run. Open a step to read what it did.",
  },

  skillsPage: {
    desc: "Shareable skill bundles installed from git repos \u2014 pick them per session.",
    installTitle: "Install a skill bundle",
    installDesc:
      "A bundle, or a marketplace of them \u2014 horsie works out which. This can take a few seconds.",
    gitUrl: "Git URL",
    gitUrlPlaceholder: "https://github.com/owner/skills-bundle",
    ref: "Ref (optional)",
    refPlaceholder: "main",
    install: "Install",
    marketplaces: "Marketplaces",
    marketplacesWhat: "marketplaces",
    marketplacesDesc:
      "Catalogues you have added. Removing one leaves its installed bundles in place.",
    installedTitle: "Installed bundles",
    installedDesc: "Toggle a bundle on to pre-select it for new sessions.",
    empty: "No skill bundles installed yet.",
  },

  memoryPage: {
    desc: "Durable notes the agent saves and reads back \u2014 grouped into spaces you pick per session.",
    spaces: "Memory spaces",
    spacesDesc:
      "A space is a namespace of memories. Sessions choose which ones they can read and write.",
    newSpace: "New space",
    newSpacePlaceholder: "ops",
    createSpaceFailed: "Failed to create space.",
    noSpaces: "No memory spaces yet. Create one above.",
    memories: "Memories",
    memoriesIn: "Memories in {{space}}",
    memoriesWhat: "memories",
    memoriesDesc:
      "The agent writes these itself. Edit or delete anything that is wrong or no longer useful.",
    createSpaceFirst: "Create a memory space first.",
    addMemory: "Add memory",
    noMemories: "No memories in this space yet.",
    memoryCount_one: "{{count}} memory",
    memoryCount_other: "{{count}} memories",
    holdsNoMemories: "It holds no memories.",
    alsoDeletes_one: "This also deletes its {{count}} memory.",
    alsoDeletes_other: "This also deletes its {{count}} memories.",
    confirmDeleteSpace: 'Delete memory space "{{name}}"? {{tail}}',
    confirmDeleteMemory: 'Delete memory "{{name}}"?',
    deleteSpace: "Delete space",
    deleteMemory: "Delete memory",
    name: "Name",
    namePlaceholder: "deploy-order",
    description: "Description",
    descriptionPlaceholder: "velos must be up before the server",
    content: "Content",
    contentPlaceholder: "Markdown. Reference another memory as [[space/name]].",
    saveMemory: "Save memory",
    saveMemoryFailed: "Failed to save memory.",
    saveChanges: "Save changes",
    updateMemoryFailed: "Failed to update memory.",
  },

  account: {
    desc: "Sign-in for this server.",
    tokens: "Machine tokens",
    tokensWhat: "machine tokens",
    tokensDesc:
      "For runtime vendor processes that run unattended. On your own machine, <cmd>horsie auth login</cmd> is enough \u2014 use a token where nobody is there to approve one. A machine token connects a runtime and can do nothing else: it cannot read sessions, change settings, or create another token.",
    tokenLabelPlaceholder: "What machine is this for?",
    copyNow: "Copy this now \u2014 it will not be shown again.",
    noTokens: "No machine tokens yet.",
    inUse: "in use",
    neverUsed: "never used",
    revoke: "Revoke",
    confirmRevoke:
      "Revoke machine token \u201c{{label}}\u201d? Anything still using it stops connecting.",
    disabled:
      "Authentication is disabled on this deployment, so there is no account to manage. Anyone who can reach this server has full access.",
    mustChange:
      "This server is still using the password it generated on first boot. Change it below \u2014 that also deletes the <file>initial-admin-password</file> file from the state directory.",
    external:
      "Sign-in for this server is managed elsewhere, so there is no password to change here.",
    currentPassword: "Current password",
    newPassword: "New password (8 characters or more)",
    passwordChanged: "Password changed. Other browsers have been signed out.",
    changePassword: "Change password",
  },

  runtimesPage: {
    vendorExists:
      "A cloud vendor named \u201c{{name}}\u201d already exists. Edit it from the list, or pick another name.",
    absentDefault:
      "Set as the default, but its agent has not connected. Sessions defaulting to it fail to start until it dials in.",
    loading: "Loading runtimes",
    loadFailed:
      "Couldn\u2019t load settings. Check that horsie-server is running, then reload.",
    desc: "Where sessions execute. Agent processes connect to this server and are configured where they run; cloud vendors are configured here.",
    vendors: "Vendors",
    vendorsDesc:
      "Run horsie connect on a machine, or start a vendor process such as horsie-velos-runtime, and it appears here. A cloud vendor needs no process of your own \u2014 each sandbox dials back to its callback URL, so that URL must be reachable from outside this server. New sessions use the default when they don\u2019t pick one.",
    empty:
      "No runtimes yet, so sessions cannot run a turn. Connect an agent, or add a cloud vendor below.",
    cloudVendors: "cloud vendors",
    checking: "Checking\u2026",
    answering: "Answering",
    connected: "Connected",
    notConnected: "Not connected",
    makeDefault: "Make {{name}} the default",
    check: "Check {{name}}",
    edit: "Edit {{name}}",
    clearDefault: "Clear the default",
    addFly: "Add Fly",
    addVelos: "Add velos",
    confirmDeleteVendor:
      "Delete cloud vendor \u201c{{name}}\u201d? Sessions that name it can no longer start.",
  },

  vendorForm: {
    name: "Name",
    flyAppPlaceholder: "horsie-runtimes",
    velosUrlPlaceholder: "http://velos.example:8080",
    regionPlaceholder: "iad",
    imagePlaceholder: "ghcr.io/you/horsie-runtime:latest",
    workspaceRootPlaceholder: "/workspaces",
    flyApp: "Fly app",
    flyAppHint:
      "Must already exist \u2014 create it with `fly apps create`. horsie only makes machines in it.",
    velosUrl: "velos server URL",
    apiToken: "API token",
    leaveBlank: "Leave blank to keep",
    flyTokenPlaceholder: "fly api token",
    velosTokenPlaceholder: "optional \u2014 velos may run without auth",
    region: "Region",
    image: "Runtime image",
    callbackUrl: "Callback URL",
    workspaceRoot: "Workspace root",
    memoryMb: "Memory (MB)",
    cpus: "CPUs",
    volumeSizeGb: "Volume size (GB)",
    volumesHint:
      "Give each runtime a volume, so a stopped one keeps its workspace",
    velosNoVolumes:
      "velos has no volumes: stopping a session deletes its container, and the next message schedules a fresh one that re-runs provisioning.",
  },

  githubApp: {
    desc: "Registration details for the GitHub App this server acts as. Set once; users then connect their own accounts from Settings \u2192 Integrations.",
    credentials: "Credentials",
    credentialsDesc:
      "From the app's page on GitHub. The secret and private key are write-only \u2014 the server reports only whether each one is set.",
    clientId: "Client ID",
    clientIdError: "The client id is what identifies the app.",
    clientSecret: "Client secret",
    appId: "App ID",
    appIdHint: "The number on the app's page on GitHub.",
    appIdError: "The App ID is the number on the app's page on GitHub.",
    privateKey: "Private key (PEM or base64)",
    privateKeyHint: "Paste the whole PEM, BEGIN and END lines included.",
    storedBlankKeeps: "\u2022\u2022\u2022\u2022 stored \u2014 blank keeps it",
    notSet: "Not set",
    saveFailed: "Failed to save.",
    configured: "App configured. <lnk>Connect an account</lnk>",
    notConfigured:
      "Not configured yet \u2014 sessions cannot clone repositories until it is.",
    callback: "Callback",
    callbackDesc:
      "Where GitHub sends users back after they authorize. horsie derives this from the request, honouring X-Forwarded-Proto, so a correctly configured reverse proxy needs nothing here. Set it when horsie cannot see its own public address \u2014 a proxy that does not forward the scheme, or a path prefix.",
    callbackBase: "Callback base URL",
    callbackPlaceholder: "https://horsie.example.com",
    callbackError: "An absolute URL, e.g. https://horsie.example.com.",
  },

  agentEdit: {
    noSuch: "No such agent: {{name}}.",
    editTitle: "Edit {{name}}",
    needName: "Give the agent a name to save it.",
    needModel: "Pick a model to save this agent.",
    saveFailed: "Failed to save agent.",
    namePlaceholder: "reviewer",
    descriptionPlaceholder: "What this agent is for",
    descriptionHint: "For the roster. The agent never sees it.",
    instructions: "Instructions",
    instructionsPlaceholder:
      "How this agent should work \u2014 added to its system prompt",
    instructionsHint:
      "Sent to the model on every turn, after the workspace's own instruction files.",
    configuration: "Configuration",
    configurationHint:
      "What every session started from this preset runs with.",
    tuning: "Tuning",
    tunable: "Let a tuning agent improve this preset",
    tunableHint:
      "A scheduled agent may read what sessions from this preset did and rewrite it \u2014 its instructions, skills, tools and memory. Off unless you turn it on.",
  },

  modelCards: {
    nameRequired: "Name is required.",
    mustBePositive: "{{label}} must be a positive whole number.",
    desc: "Well-known models and their token limits. Settings \u2192 Models autocompletes model ids from these and prefills empty limit fields; editing a card never changes an already-configured model.",
    catalog: "Catalog",
    catalogDesc: "One entry per well-known model.",
    empty: "No model cards.",
    loadFailed: "Couldn\u2019t load model cards.",
    filterPlaceholder: "Filter by model id or name\u2026",
    filterLabel: "Filter model cards",
    ctx: "{{value}} ctx",
    out: "{{value}} out",
    detailsFor: "Details for {{name}}",
    modelId: "Model id",
    modelIdPlaceholder: "claude-sonnet-4-6",
    namePlaceholder: "Claude Sonnet 4.6",
    contextWindow: "Context window",
    contextWindowOptional: "Context window (optional)",
    maxTokens: "Max tokens",
    maxTokensOptional: "Max tokens (optional)",
    baseUrl: "Base URL",
    baseUrlOptional: "Base URL (optional)",
    baseUrlPlaceholder: "https://api.deepseek.com",
    thinkingDialect: "Thinking dialect",
    thinkingDialectOptional: "Thinking dialect (optional)",
    thinkingEfforts: "Thinking efforts",
    thinkingEffortsOptional: "Thinking efforts (optional)",
    thinkingEffortsHint:
      "What this model accepts, ascending. Leave empty for a model with no thinking control.",
    defaultEffort: "Default effort",
    defaultEffortOptional: "Default thinking effort (optional)",
    forcedTools: "Pinned tool choice disables thinking",
    supportsImages: "Can be shown images",
    supportsDocuments: "Can be shown documents (PDF)",
    forcedToolsHint:
      "For backends that reject a forced <mono>tool_choice</mono> while thinking is on \u2014 DeepSeek answers 400 \u201cThinking mode does not support this tool_choice\u201d.",
    addCard: "Add card",
    confirmDelete:
      'Delete model card "{{name}}"? Models already configured keep their current values.',
  },

  environmentEdit: {
    noSuch: "No such environment: {{name}}.",
    needName: "Give the environment a name to save it.",
    needVendor: "Choose the runtime vendor this environment runs on.",
    saveFailed: "Failed to save environment.",
    namePlaceholder: "staging",
    descriptionPlaceholder: "What this environment is for",
    vendor: "Runtime vendor",
    selectVendor: "Select a runtime vendor",
    vendorNotConnected: "{{name}} \u2014 not connected",
    vendorHint:
      "Only vendors that provision their own workspace can run an environment, so local runtimes are not listed.",
    noProvisioningVendor:
      "No connected vendor provisions its own workspace, so nothing can run an environment yet. Add one under <lnk>Settings \u203a Runtimes</lnk>.",
    envVars: "Env vars",
    envVarsHint: "Plain text only \u2014 no secrets here.",
    envVarName: "NAME",
    envVarValue: "value",
    removeEnvVar: "Remove env var",
    addEnvVar: "Add env var",
    provision: "Provision steps",
    provisionHint:
      "A JSON array of {name, uses, with} steps. Nothing runs them yet.",
    provisionInvalid:
      "Provision steps must be a JSON array of {name, uses, with}.",
    reposFromGithub:
      "Repos come from your GitHub App installation. <lnk>Connect GitHub</lnk> to pick them.",
    loadingRepos: "Loading repos\u2026",
    noReposVisible:
      "No repos are visible to the app installation. <lnk>Check its access</lnk>.",
    filterRepos: "Filter repos",
    notInInstallation: "not in installation",
    ref: "ref",
    gitRefFor: "Git ref for {{name}}",
    noRepoMatches: "No repo matches \u201c{{query}}\u201d.",
  },

  routineEdit: {
    saveFailed: "Failed to save routine.",
    namePlaceholder: "nightly-triage",
    descriptionPlaceholder: "What this routine is for",
    chooseAgent: "Choose an agent\u2026",
    agentHint:
      "The routine runs with this agent\u2019s model, skills and memory. Edit those on the Agents page.",
    environmentHint:
      "Where every run happens. A run whose environment has gone \u2014 an offline runtime, a deleted environment \u2014 fails and says so here.",
    promptPlaceholder:
      "Everything the run gets told. It cannot ask you a question, so say what to do when a choice comes up.",
    trigger: "Trigger",
    kindManual: "Only when I run it",
    kindEvery: "Repeatedly",
    kindOnce: "Once, at a time",
    kindDaily: "Daily, at a time",
    kindWeekly: "Weekly, on chosen days",
    kindMonthly: "Monthly, on a day",
    kindYearly: "Yearly, on a date",
    everyLabel: "every",
    minutes: "minutes",
    atLabel: "at",
    onLabel: "on",
    onTheLabel: "on the",
    dayLabel: "day",
    browserTimezone: "Browser timezone",
    customTimezone: "Custom timezone",
    timezone: "Timezone",
    done: "Done",
    change: "Change",
    daysOfWeek: "Days of week",
    weekdays: "Weekdays",
    pickADay: "Pick at least one day.",
    shortestInterval_one: "The shortest interval is {{count}} minute.",
    shortestInterval_other: "The shortest interval is {{count}} minutes.",
    timerActive: "Timer active",
    timerHint:
      "The run button and the API work either way \u2014 pausing only stops the timer. Runs are not prevented from overlapping, so leave the interval room to finish.",
  },

  stepForm: {
    namePlaceholder: "step name",
    missingAgent:
      "No agent named <name>{{agent}}</name> exists any more, so this step fails when the workflow runs. Pick another, or recreate it.",
    promptPlaceholder:
      "What this step should do. Its input is appended below it.",
    outcomes: "Outcomes",
    outcomesHint:
      "How this step can end. The step picks one, and it is the only thing a transition reads. Each needs a description \u2014 it is what the model reads to choose between them.",
    outcomePlaceholder: "success",
    outcomeDescPlaceholder: "what it means",
    removeOutcome: "Remove outcome {{name}}",
    addOutcome: "Add outcome",
    fields: "Result fields",
    fieldNamePlaceholder: "severity",
    fieldDescPlaceholder: "what it holds",
    typeString: "string",
    typeNumber: "number",
    typeBoolean: "boolean",
    typeStringList: "string list",
    required: "required",
    removeField: "Remove field {{name}}",
    addField: "Add field",
    canAsk: "Can ask the person",
    canAskHint:
      "Gives this step the ask_user tool. Without it the step has no way to ask, and must decide for itself.",
    goesTo: "Goes to",
    goesToHint:
      "Tried in order; the first match wins. A row that names no outcome is the catch-all. No match ends the run.",
    opAlways: "always",
    opIn: "outcome in",
    opNotIn: "outcome not in",
    chooseStep: "Choose a step\u2026",
    removeTransition: "Remove transition {{n}}",
    addTransition: "Add transition",
    limits: "Limits",
    maxIterations: "Max iterations",
    unlimited: "unlimited",
    retries: "Retries",
    limitsHint:
      "How many turns this step may take before it fails, and how many times a transient provider error is retried within it. Leave both blank for the defaults.",
  },

  workflowEdit: {
    reorder: "Reorder {{name}} with the arrow keys",
    removeStep: "Remove step {{name}}",
    stepFallback: "step",
    noSuch: "No such workflow: {{name}}.",
    contents: "Workflow contents",
    definition: "Definition",
    unnamed: "unnamed",
    addStep: "Add step",
    visualize: "Visualize",
    chooseStep: "Choose a step to edit it.",
    namePlaceholder: "fix-bug",
    stepBudget: "Step budget",
    stepBudgetPlaceholder: "100 (default)",
    stepBudgetHint:
      "Most steps one run may execute. This is what stops a loop whose condition never flips; raise it for a graph that legitimately loops far.",
    startsAt: "Starts at",
    chooseAStep: "\u2014 choose a step \u2014",
    noSuchStep: "{{name}} (no such step)",
  },

  modelsPage: {
    addProvider: "Add provider",
    addModel: "Add model",
    saveProvider: "Save provider",
    saveModel: "Save model",
    noProviders: "No providers yet.",
    noModelsFor: "No models route through {{provider}} yet.",
    title: "Models & providers",
    desc: "API endpoints and the model aliases sessions pick from. Each provider and each model saves on its own \u2014 open one, edit it, press its Save.",
    loadFailed: "Couldn\u2019t load settings. Is <cmd>horsie serve</cmd> running?",
    providers: "Providers",
    providersDesc:
      "API endpoints. Select one to see the models routed through it.",
    kind: {
      anthropic: "Anthropic",
      openai: "OpenAI-compatible",
      "openai-responses": "OpenAI Responses",
      chatgpt: "ChatGPT plan",
    },
    kindLabel: "Kind",
    keySet: "Key set",
    noKey: "No key",
    signedInHint: "Signed in to a ChatGPT plan.",
    notSignedInHint:
      "Not signed in \u2014 connect this provider before adding models to it.",
    keyStoredHint: "An API key is stored for this provider.",
    noKeyHint: "No API key stored \u2014 add one before adding models to it.",
    selectProviderFirst: "Select a provider first.",
    connectFirst: "Connect \u201c{{name}}\u201d to a ChatGPT plan first.",
    addKeyFirst: "Add an API key to \u201c{{name}}\u201d first.",
    needProviderName: "Every provider needs a name.",
    providerExists: "A provider named \u201c{{name}}\u201d already exists.",
    confirmDeleteProvider: "Delete provider \u201c{{name}}\u201d?",
    confirmDeleteProviderWithModels_one:
      "Delete provider \u201c{{name}}\u201d and its model ({{aliases}})?",
    confirmDeleteProviderWithModels_other:
      "Delete provider \u201c{{name}}\u201d and its {{count}} models ({{aliases}})?",
    needAlias: "Every model needs an alias.",
    aliasExists: "A model aliased \u201c{{alias}}\u201d already exists.",
    mustBeNumber: "{{label}} for \u201c{{alias}}\u201d must be a number.",
    confirmDeleteModel: "Delete model \u201c{{alias}}\u201d?",
    modelCount_one: "{{count}} model",
    modelCount_other: "{{count}} models",
    chatgptSignInFor: "ChatGPT sign-in for {{name}}",
    connect: "Connect",
    modelsFor: "Models \u00b7 {{provider}}",
    modelsDesc:
      "Aliases sessions pick from. Each routes to a model id on this provider.",
    providerNamePlaceholder: "anthropic",
    baseUrlHint:
      "Host only \u2014 horsie appends the API path. {{example}}, not {{example}}/v1.",
    inlineKey: "Inline key",
    willBeCleared: "will be cleared on save",
    notSetLower: "not set",
    clearKey: "Clear the stored key on save",
    chatgptHint:
      "A ChatGPT plan is authorized by signing in, not by a key. Connect it from its row in the list.",
    chatgptHintNew:
      "A ChatGPT plan is authorized by signing in, not by a key. Connect it from its row in the list once this is saved.",
    keepSignatures: "Keep thinking signatures",
    keepSignaturesHint:
      "Required for api.anthropic.com, which validates them on replay. Leave off for Anthropic-compatible endpoints \u2014 the blobs are several KB per thinking block and nothing reads them.",
    alias: "Alias",
    aliasPlaceholder: "sonnet",
    provider: "Provider",
    thinkingEfforts: "Thinking efforts this model offers",
    wireDialect: "Wire dialect",
    forcedToolsHint:
      "Required for DeepSeek, which rejects a forced tool choice while thinking is on. Sub-agents that must call a handoff tool will run without thinking.",
    visionHint:
      "Attachments are only loaded for a model that takes them. A model with neither box ticked is told an attachment was withheld, instead of being sent bytes it cannot read.",
  },

  mcpChannel: {
    allTools: "all",
    noTools: "This server advertised no tools.",
    toolsUnreadable: "Could not read this server's tools.",
    toolCount_one: "{{count}} tool",
    toolCount_other: "{{count}} tools",
  },
  integrations: {
    desc: "GitHub, MCP servers, and this server's build info.",
    github: "GitHub",
    githubDesc:
      "Connect your GitHub account so sessions can clone your repositories.",
    connectedAs: "Connected as <login>@{{login}}</login>",
    disconnect: "Disconnect",
    appConfigured: "App configured \u2014 connect your account.",
    noApp:
      "No GitHub App is registered on this server yet. Set one up in <lnk>Admin \u2192 GitHub App</lnk>.",
    registerFirst: "Register the GitHub App in Admin first",
    githubTools: "GitHub tools (MCP)",
    githubToolsDesc:
      "Let sessions call the GitHub MCP server (create PRs, search issues\u2026) using this connection.",
    enable: "Enable",
    disable: "Disable",
    test: "Test",
    testFailed: "Test failed.",
    enabledTools_one: "enabled \u00b7 {{count}} tool",
    enabledTools_other: "enabled \u00b7 {{count}} tools",
    enabled: "enabled",
    toolCount_one: "{{count}} tool",
    toolCount_other: "{{count}} tools",
    description: "Description",
    descriptionPlaceholder: "what this server is for",
    descriptionHint:
      "Shown wherever this server is listed. Leave it blank to use what the server says about itself.",
    serverInstructions: "What the server says",
    tools: "the tool list",
    noTools: "This server advertised no tools.",
    noToolDescription: "no description",
    notTested: "not tested",
    mcpDesc:
      "Remote Model Context Protocol servers. Sessions pick which to use; their tools appear as <mono/>.",
    addServer: "Add server",
    noServers: "No MCP servers configured.",
    namePlaceholder: "linear",
    nameHint:
      "Letters, digits, '-' and '_'. It becomes part of every tool id: mcp__<name>__<tool>.",
    url: "URL",
    urlPlaceholder: "https://mcp.example.com/",
    auth: "Auth",
    authNone: "None (public)",
    authBearer: "Bearer token",
    authOAuth: "OAuth 2.1",
    clientId: "Client ID (optional)",
    clientSecret: "Client secret (optional)",
    autoRegister: "blank = auto-register",
    authorized: "authorized",
    connectFailed: "Connect failed.",
    reauthorize: "Reauthorize",
    server: "Server",
    none: "(none)",
    configFile: "Config file",
    database: "Database",
    stateDir: "State dir",
    dataDir: "Data dir",
    pluginsDir: "Plugins dir",
    version: "Version",
  },

  settingsMenu: {
    title: "What this panel shows",
    ariaLabel: "Display options",
    heading: "Display",
  },

  appearance: {
    title: "Appearance",
    desc: "How this browser renders horsie. Stored locally, not on the server, so each browser you use can differ.",
    themeTitle: "Theme",
    themeDesc:
      "Same layouts, different material. Every theme ships light and dark, and every one is measured to WCAG AA in both.",
    themeGroup: "Theme",
    modeTitle: "Light or dark",
    modeDesc:
      "System follows your operating system and keeps following it while this tab is open.",
    modeGroup: "Mode",
    modeLight: "Light",
    modeDark: "Dark",
    modeSystem: "System",
    textSizeTitle: "Text size",
    textSizeDesc:
      "Scales every measurement in the interface, so the spacing grows with the type rather than the type outgrowing its slots.",
    textSizeGroup: "Text size",
    transcriptTitle: "Transcript",
    transcriptDesc:
      "What the session view shows. These are display switches, not session settings — they change nothing about how the agent runs.",
    languageTitle: "Language",
    languageDesc:
      "The language this interface is written in. System follows your browser and keeps following it.",
    languageGroup: "Language",
    languageSystem: "System",
    languageSystemNote: "Follow the browser",
    skin: {
      paper: {
        name: "Paper",
        blurb:
          "Warm all the way down — bone in the light, warm charcoal in the dark, one vermillion for the control that commits.",
      },
      signal: {
        name: "Signal",
        blurb:
          "The cold opposite — a blue-black ground under a single lime accent. Same layout, other temperature.",
      },
    },
    textSize: {
      compact: {
        name: "Compact",
        blurb: "The densest fit — most transcript on screen.",
      },
      default: { name: "Default", blurb: "The shipped density." },
      large: {
        name: "Large",
        blurb: "Roomier type and spacing, less on screen at once.",
      },
    },
  },
};

export default en;
