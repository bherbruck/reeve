// SSE -> TanStack Query cache invalidation.
//
// One EventSource per app to /api/reeve/v1/events. Events are
// cache-invalidation HINTS, never a data channel: each typed event
// maps to generated query-key factories (the ONLY query keys the UI
// uses) and the data of record is refetched from the REST API.
// A lost event costs latency, never correctness: reconnects replay
// via `Last-Event-ID` (the browser sends it natively), `reset` and
// reconnect both invalidate everything, and every view keeps a
// polling fallback (`usePollInterval` below).
import { useSyncExternalStore } from 'react'
import type { QueryClient } from '@tanstack/react-query'
import { getEventsRouteUrl } from '@/api/endpoints/events/events'
import {
  getDetailQueryKey,
  getJournalQueryKey,
  getListQueryKey,
} from '@/api/endpoints/devices/devices'
import { getListDeployLogsQueryKey } from '@/api/endpoints/logs/logs'
import { getDurabilityStatusQueryKey } from '@/api/endpoints/durability/durability'
import {
  getFederationStatusQueryKey,
  getListTokensRouteQueryKey,
} from '@/api/endpoints/federation/federation'
import {
  getListRolloutsQueryKey,
  getRolloutStatusQueryKey,
} from '@/api/endpoints/rollouts/rollouts'
import { getListSecretsQueryKey } from '@/api/endpoints/secrets/secrets'
import type {
  DeploymentStatusEvent,
  DevicePresenceEvent,
  HealthStateEvent,
  RolloutEvent,
  SecretRotationEvent,
  TerminalSessionEvent,
} from '@/api/model'

// ---- connection state store (freshness indicator + polling fallback) ----

let connected = false
const listeners = new Set<() => void>()

function setConnected(value: boolean) {
  if (connected === value) return
  connected = value
  listeners.forEach((l) => l())
}

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

/** Whether the live event stream is currently open. */
export function useSseConnected(): boolean {
  return useSyncExternalStore(subscribe, () => connected)
}

/**
 * Polling fallback: every view stays correct with the stream absent
 * (typically 30 s for lists, 10 s for focused detail). While SSE is up,
 * invalidation drives freshness and polling rests.
 */
export function usePollInterval(whenDownMs: number): number | false {
  const up = useSseConnected()
  return up ? false : whenDownMs
}

// ---- event -> generated-query-key invalidation map ----

function invalidateDevice(qc: QueryClient, deviceId: string) {
  void qc.invalidateQueries({ queryKey: getListQueryKey() })
  void qc.invalidateQueries({ queryKey: getDetailQueryKey(deviceId) })
}

type Handler = (qc: QueryClient, data: string) => void

function parse<T>(data: string): T | undefined {
  try {
    return JSON.parse(data) as T
  } catch {
    return undefined
  }
}

const handlers: Record<string, Handler> = {
  // Replay was not possible — treat ALL cached state as stale.
  reset: (qc) => {
    void qc.invalidateQueries()
  },
  'device-presence': (qc, data) => {
    const e = parse<DevicePresenceEvent>(data)
    if (e) invalidateDevice(qc, e.deviceId)
  },
  'deployment-status': (qc, data) => {
    const e = parse<DeploymentStatusEvent>(data)
    if (!e) return
    invalidateDevice(qc, e.deviceId)
    // Prefix-invalidates every loaded journal page for the device.
    void qc.invalidateQueries({ queryKey: getJournalQueryKey(e.deviceId) })
    // Prefix-invalidates the deploy-log list for every deployment on
    // the device (the key omits the `deployment` param), so a fresh
    // failure surfaces its captured output without a manual reload.
    void qc.invalidateQueries({
      queryKey: getListDeployLogsQueryKey(e.deviceId),
    })
  },
  'health-state': (qc, data) => {
    const e = parse<HealthStateEvent>(data)
    if (!e) return
    invalidateDevice(qc, e.deviceId)
    void qc.invalidateQueries({ queryKey: getJournalQueryKey(e.deviceId) })
  },
  'terminal-session': (qc, data) => {
    const e = parse<TerminalSessionEvent>(data)
    if (e) void qc.invalidateQueries({ queryKey: getDetailQueryKey(e.deviceId) })
  },
  'verify-restore': (qc) => {
    void qc.invalidateQueries({ queryKey: getDurabilityStatusQueryKey() })
  },
  'durability-lag': (qc) => {
    void qc.invalidateQueries({ queryKey: getDurabilityStatusQueryKey() })
  },
  rollout: (qc, data) => {
    const e = parse<RolloutEvent>(data)
    void qc.invalidateQueries({ queryKey: getListRolloutsQueryKey() })
    if (e)
      void qc.invalidateQueries({ queryKey: getRolloutStatusQueryKey(e.rolloutId) })
  },
  'secret-rotation': (qc, data) => {
    const e = parse<SecretRotationEvent>(data)
    void qc.invalidateQueries({ queryKey: getListSecretsQueryKey() })
    if (e && e.state === 'converged')
      // Converged rotations mean device manifests advanced.
      void qc.invalidateQueries({ queryKey: getListQueryKey() })
  },
  'federation-sync': (qc) => {
    void qc.invalidateQueries({ queryKey: getFederationStatusQueryKey() })
    void qc.invalidateQueries({ queryKey: getListTokensRouteQueryKey() })
  },
}

// ---- lifecycle ----

let source: EventSource | null = null
let refs = 0

/**
 * Start (or share) the app-wide event stream. Returns a stop handle;
 * the stream closes when the last consumer stops. The browser's
 * EventSource reconnects automatically and sends `Last-Event-ID` —
 * no hand-rolled retry loop.
 */
export function startSse(queryClient: QueryClient): () => void {
  refs += 1
  if (!source) {
    // Generated URL builder — never a hand-written path.
    const es = new EventSource(getEventsRouteUrl())
    source = es

    let droppedSinceOpen = false
    es.onopen = () => {
      setConnected(true)
      if (droppedSinceOpen) {
        // Anything may have happened while we were away; if the
        // server could not replay it also sent `reset`, but a plain
        // reconnect still refetches truth.
        void queryClient.invalidateQueries()
      }
      droppedSinceOpen = false
    }
    es.onerror = () => {
      droppedSinceOpen = true
      setConnected(false)
    }
    for (const [type, handler] of Object.entries(handlers)) {
      es.addEventListener(type, (ev) => {
        handler(queryClient, (ev as MessageEvent<string>).data)
      })
    }
  }
  let stopped = false
  return () => {
    if (stopped) return
    stopped = true
    refs -= 1
    if (refs <= 0 && source) {
      source.close()
      source = null
      setConnected(false)
    }
  }
}
