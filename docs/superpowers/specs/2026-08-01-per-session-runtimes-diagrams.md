# Per-session local runtimes — diagrams

Paste any block below into Excalidraw: hamburger menu → Import → Mermaid.

## 1. The two chains

Every path is a **lifecycle** chain (who makes a runtime exist) plus a **data**
chain (how tool calls reach it). They are independent.

```mermaid
flowchart TB
  subgraph LIFECYCLE["LIFECYCLE — who makes a runtime exist"]
    V["RuntimeVendor<br/>server, per session"]
    EC["ExecutorClient<br/>create / attach / stop"]
    ET{{"ExecutorTransport<br/>trait"}}
    INMEM["InMemExecutorTransport<br/>in-process"]
    WS["WsExecutorTransport<br/>remote — deleted by #83"]
    RP{{"RuntimeProvider<br/>trait"}}
    PROC["ProcessRuntimeProvider<br/>spawn a child"]
    VELOS["VelosRuntimeProvider<br/>schedule a container"]
    RH["RuntimeHandle<br/>stop / health_check"]
    RR["RuntimeRegistry<br/>id → state + handle"]

    V --> EC --> ET
    ET --> INMEM
    ET --> WS
    INMEM --> RP
    RP --> PROC
    RP --> VELOS
    PROC --> RH
    VELOS --> RH
    INMEM -.owns.-> RR
  end

  subgraph DATA["DATA — how tool calls reach the runtime"]
    RC["RuntimeClient<br/>server, per session"]
    RT{{"RuntimeTransport<br/>trait"}}
    SOCK["SocketRuntimeTransport<br/>direct WS / unix"]
    RELAY["RelayRuntimeTransport<br/>tunneled — deleted by #83"]
    CRR["ConnectedRuntimeRegistry<br/>id → live transport<br/>+ readiness waiters"]
    HRC["handle_runtime_connection<br/>handshake + register"]

    RC --> RT
    RT --> SOCK
    RT --> RELAY
    CRR --> SOCK
    HRC --> CRR
  end

  WS -.provides.-> RELAY
```

## 2. Wiring today — velos

Provider on the server; tool calls go straight to the container.

```mermaid
flowchart LR
  S["session"] --> VV["VelosVendor"]
  VV --> EC["ExecutorClient"] --> IM["InMemExecutorTransport"] --> VP["VelosRuntimeProvider"]
  VP -->|schedule| API["velos API"]
  API --> C["container<br/>horsie-runtime"]
  C -->|"WS dial-back<br/>/api/runtime/connect"| CRR["ConnectedRuntimeRegistry"]
  S --> RC["RuntimeClient"] --> SOCK["SocketRuntimeTransport"]
  CRR --> SOCK
  SOCK -->|tool calls| C
```

## 3. Wiring today — local (no lifecycle at all)

`horsie connect` spawns one runtime that registers *itself* as a vendor.
Nothing creates runtimes; `create`/`attach` are lookups, `stop`/`delete` are no-ops.

```mermaid
flowchart LR
  HC["horsie connect"] -->|spawn| RT["horsie-runtime<br/>one process, fixed dir"]
  RT -->|"WS<br/>/api/runtime/connect?register=local"| CRR["ConnectedRuntimeRegistry"]
  CRR --> HOOK["ConnectHook"] --> LDV["LocalDaemonVendor<br/>label = a vendor"]
  S1["session A"] --> LDV
  S2["session B"] --> LDV
  LDV -->|"lookup only"| SOCK["SocketRuntimeTransport"]
  SOCK -->|tool calls, shared| RT
```

## 4. Wiring today — CLI workflow jobs

In-process lifecycle, children on a unix socket. This is the shape the new
design reuses on the user's machine.

```mermaid
flowchart LR
  J["job actor"] --> EC["ExecutorClient"] --> IM["InMemExecutorTransport"] --> PP["ProcessRuntimeProvider"]
  PP -->|spawn| CH["horsie-runtime child"]
  CH -->|"unix WS dial-in"| CRR["ConnectedRuntimeRegistry"]
  CRR --> SOCK["SocketRuntimeTransport"]
  SOCK -->|tool calls| CH
```

## 5. Target design — one WS per machine, children behind the executor

Wiring 4 on the user's machine, driven remotely through wiring 1's transport.
The server-facing connection belongs to the executor, so it survives killing and
respawning any child behind it — that is what makes hibernate/resume possible.

```mermaid
flowchart TB
  subgraph SERVER["server"]
    SA["session A"]
    SB["session B"]
    EV["ExecutorVendor"]
    EC["ExecutorClient"]
    WS["WsExecutorTransport"]
    RC["RuntimeClient<br/>per session"]
    RELAY["RelayRuntimeTransport"]
    REG["executor registry<br/>label → transport"]

    SA --> EV
    SB --> EV
    EV --> EC --> WS
    SA --> RC --> RELAY
    WS -.provides.-> RELAY
    REG -.holds.-> WS
  end

  WS <==>|"ONE WS per machine<br/>/api/executor/connect<br/>lifecycle + tool calls,<br/>correlated by request_id / call_id"| DISP

  subgraph MACHINE["user machine — horsie connect"]
    DISP["dispatch"]
    EC2["ExecutorClient"]
    IM["InMemExecutorTransport"]
    PP["ProcessRuntimeProvider"]
    CRR["ConnectedRuntimeRegistry"]
    SOCK["SocketRuntimeTransport"]
    CA["runtime child<br/>session A"]
    CB["runtime child<br/>session B"]

    DISP -->|lifecycle| EC2 --> IM --> PP
    PP -->|spawn| CA
    PP -->|spawn| CB
    CA -->|unix WS| CRR
    CB -->|unix WS| CRR
    CRR --> SOCK
    DISP -->|tool calls| SOCK
    SOCK --> CA
    SOCK --> CB
  end
```

## 6. What the design removes

```mermaid
flowchart LR
  subgraph GONE["deleted by this design"]
    A["LocalDaemonVendor"]
    B["LocalDaemonRegistry"]
    C["ConnectHook"]
    D["?register=local branch"]
  end
  subgraph BACK["restored from #83, becomes live"]
    E["WsExecutorTransport<br/>+ RelayRuntimeTransport"]
    F["Executor + dispatch"]
  end
  subgraph STAYS["correctly deleted, stays deleted"]
    G["server::Server"]
    H["ExecutorRegistry / CommandSink"]
    I["ExecutorEventHandler"]
  end
```
