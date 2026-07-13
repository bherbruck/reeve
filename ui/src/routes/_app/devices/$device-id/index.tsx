import { useState, type ReactNode } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft, Pencil, Pin, Rocket } from 'lucide-react'
import { useMe } from '@/api/endpoints/auth/auth'
import { useDetail, useJournal } from '@/api/endpoints/devices/devices'
import type {
  ComponentStatus,
  DeploymentStatusManifest,
  DeviceDetail,
  JournalEntry,
} from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { DeployLogsDialog } from '@/components/deploy-logs-dialog'
import { DeploymentStateBadge } from '@/components/deployment-state-badge'
import { DeviceTerminal } from '@/components/device-terminal'
import { PresenceBadge } from '@/components/presence-badge'
import { fmtRfc3339, fmtUnix } from '@/lib/format'
import { usePollInterval } from '@/lib/sse'
import { cn } from '@/lib/utils'

export const Route = createFileRoute('/_app/devices/$device-id/')({
  component: DeviceDetailPage,
})

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="text-sm">{children}</span>
    </div>
  )
}

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono text-xs">{children}</span>
}

/** Failure reason + per-component states for one deployment. */
type DeploymentReport = {
  errorMessage: string | null
  components: ComponentStatus[]
}

/**
 * Best-effort per-deployment failure detail keyed by deployment id.
 * The device-detail API's current-state rows carry only the Margo
 * `state`, not `status.error` or per-component states — but the status
 * journal records the full DeploymentStatusManifest, so we read the
 * newest status record per deployment from the head journal page (a
 * bounded, forensic source; shares the Journal tab's cached page).
 */
function useDeploymentReports(
  deviceId: string,
): Map<string, DeploymentReport> {
  const refetchInterval = usePollInterval(10_000)
  const page = useJournal(
    deviceId,
    { limit: JOURNAL_PAGE_SIZE },
    { query: { refetchInterval } },
  )
  const reports = new Map<string, DeploymentReport>()
  if (page.data?.status !== 200) return reports
  for (const record of page.data.data.records) {
    if (record.kind !== 'status' || record.payload == null) continue
    const manifest = record.payload as DeploymentStatusManifest
    if (
      typeof manifest.deploymentId !== 'string' ||
      reports.has(manifest.deploymentId)
    )
      continue
    reports.set(manifest.deploymentId, {
      errorMessage: manifest.status?.error?.message ?? null,
      components: manifest.components ?? [],
    })
  }
  return reports
}

/** Per-component state pill; carries its error message as a tooltip. */
function ComponentBadge({ component }: { component: ComponentStatus }) {
  const tone =
    component.state === 'installed'
      ? 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400'
      : component.state === 'failed'
        ? 'border-red-500/40 text-red-600 dark:text-red-400'
        : 'text-muted-foreground'
  return (
    <Badge
      variant="outline"
      title={component.error?.message ?? undefined}
      className={cn('font-mono text-xs font-normal', tone)}
    >
      {component.name}: {component.state}
    </Badge>
  )
}

function OverviewTab({ device }: { device: DeviceDetail }) {
  const tags = Object.entries(device.tags)
  const configApps = device.render?.apps ?? []
  const reports = useDeploymentReports(device.deviceId)
  return (
    <div className="flex flex-col gap-4">
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Details</CardTitle>
        </CardHeader>
        <CardContent className="grid grid-cols-2 gap-4 md:grid-cols-4">
          <Field label="Display name">{device.displayName ?? '—'}</Field>
          <Field label="Hostname">{device.hostname}</Field>
          <Field label="Device id">
            <Mono>{device.deviceId}</Mono>
          </Field>
          <Field label="Architecture">{device.arch}</Field>
          <Field label="Fleet">{device.fleet ?? '—'}</Field>
          <Field label="Site">{device.site ?? '—'}</Field>
          <Field label="Device type">{device.type ?? '—'}</Field>
          <Field label="Pinned">
            {device.pinned ? (
              <Badge variant="outline" className="gap-1 font-normal">
                <Pin className="size-3" /> pinned
              </Badge>
            ) : (
              '—'
            )}
          </Field>
          <Field label="Agent version">{device.agentVersion}</Field>
          <Field label="Enrolled">{fmtUnix(device.enrolledAt)}</Field>
          <Field label="Last seen">{fmtUnix(device.lastSeenAt)}</Field>
          <Field label="Tags">
            {tags.length === 0 ? (
              '—'
            ) : (
              <span className="flex flex-wrap gap-1">
                {tags.map(([k, v]) => (
                  <Badge
                    key={k}
                    variant="secondary"
                    className="font-mono text-xs font-normal"
                  >
                    {k}
                    {v ? `=${v}` : ''}
                  </Badge>
                ))}
              </span>
            )}
          </Field>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Deployments</CardTitle>
          <CardDescription>
            Current per-deployment state as last reported by the device.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {device.deployments.length === 0 ? (
            <p className="text-sm text-muted-foreground">No deployments.</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Deployment</TableHead>
                  <TableHead>State</TableHead>
                  <TableHead>Observed</TableHead>
                  <TableHead>Received</TableHead>
                  <TableHead className="text-right">Logs</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {device.deployments.map((d) => {
                  const report = reports.get(d.deploymentId)
                  const showError =
                    d.state === 'failed' && !!report?.errorMessage
                  return (
                    <TableRow key={d.deploymentId}>
                      <TableCell className="align-top">
                        <div className="flex flex-col gap-1.5">
                          <Mono>{d.deploymentId}</Mono>
                          {showError && (
                            <span className="text-xs text-red-600 dark:text-red-400">
                              {report?.errorMessage}
                            </span>
                          )}
                          {report && report.components.length > 0 && (
                            <span className="flex flex-wrap gap-1">
                              {report.components.map((c) => (
                                <ComponentBadge key={c.name} component={c} />
                              ))}
                            </span>
                          )}
                        </div>
                      </TableCell>
                      <TableCell className="align-top">
                        <DeploymentStateBadge state={d.state} />
                      </TableCell>
                      <TableCell className="align-top text-sm text-muted-foreground">
                        {fmtRfc3339(d.observedAt)}
                      </TableCell>
                      <TableCell className="align-top text-sm text-muted-foreground">
                        {fmtUnix(d.receivedAt)}
                      </TableCell>
                      <TableCell className="align-top text-right">
                        <DeployLogsDialog
                          deviceId={device.deviceId}
                          deploymentId={d.deploymentId}
                        />
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Configuration</CardTitle>
          <CardDescription>
            The apps this device is currently assigned.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {!device.render ? (
            <p className="text-sm text-muted-foreground">
              No configuration has been prepared for this device yet.
            </p>
          ) : configApps.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              No apps assigned.{' '}
              <span className="text-muted-foreground/80">
                Last updated {fmtUnix(device.render.updatedAt)}.
              </span>
            </p>
          ) : (
            <div className="flex flex-col gap-3">
              <span className="flex flex-wrap gap-1">
                {configApps.map((a) => (
                  <Badge key={a.appId} variant="secondary" className="font-normal">
                    {a.appId}
                  </Badge>
                ))}
              </span>
              <span className="text-xs text-muted-foreground">
                Last updated {fmtUnix(device.render.updatedAt)}.
              </span>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  )
}

function JournalRecordRow({ record }: { record: JournalEntry }) {
  return (
    <div className="flex flex-col gap-1 border-b px-4 py-3 last:border-b-0">
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <Badge variant="outline" className="font-normal">
          {record.kind}
        </Badge>
        <span>seq {record.seq}</span>
        <span>observed {fmtRfc3339(record.observedAt)}</span>
        <span>received {fmtUnix(record.receivedAt)}</span>
      </div>
      {record.payload != null && (
        <pre className="overflow-x-auto rounded bg-muted p-2 font-mono text-xs">
          {JSON.stringify(record.payload, null, 2)}
        </pre>
      )}
    </div>
  )
}

const JOURNAL_PAGE_SIZE = 50

/** One fetched page; the last page offers "load older" via nextBeforeSeq. */
function JournalPageBlock({
  deviceId,
  beforeSeq,
  isLast,
  onLoadOlder,
}: {
  deviceId: string
  beforeSeq: number | undefined
  isLast: boolean
  onLoadOlder: (nextBeforeSeq: number) => void
}) {
  const refetchInterval = usePollInterval(10_000)
  const page = useJournal(
    deviceId,
    { limit: JOURNAL_PAGE_SIZE, before_seq: beforeSeq },
    // Only the newest page live-updates; older pages are immutable.
    { query: { refetchInterval: beforeSeq == null ? refetchInterval : false } },
  )

  if (page.isLoading)
    return <p className="px-4 py-3 text-sm text-muted-foreground">Loading…</p>
  if (!page.data || page.data.status !== 200)
    return (
      <p className="px-4 py-3 text-sm text-destructive">
        Could not load journal page.
      </p>
    )
  const { records, nextBeforeSeq } = page.data.data
  return (
    <>
      {records.length === 0 && beforeSeq == null ? (
        <p className="px-4 py-3 text-sm text-muted-foreground">
          The journal is empty.
        </p>
      ) : (
        records.map((r) => <JournalRecordRow key={r.seq} record={r} />)
      )}
      {isLast && nextBeforeSeq != null && (
        <div className="p-3">
          <Button
            variant="outline"
            size="sm"
            onClick={() => onLoadOlder(nextBeforeSeq)}
          >
            Load older records
          </Button>
        </div>
      )}
    </>
  )
}

function JournalTab({ deviceId }: { deviceId: string }) {
  // Newest page first; each "load older" pins another page by its
  // before_seq cursor (server pages are stable snapshots by seq).
  const [olderCursors, setOlderCursors] = useState<number[]>([])
  const cursors: (number | undefined)[] = [undefined, ...olderCursors]
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Status journal</CardTitle>
        <CardDescription>Newest first.</CardDescription>
      </CardHeader>
      <CardContent className="p-0">
        {cursors.map((cursor, i) => (
          <JournalPageBlock
            key={cursor ?? 'head'}
            deviceId={deviceId}
            beforeSeq={cursor}
            isLast={i === cursors.length - 1}
            onLoadOlder={(next) => setOlderCursors((prev) => [...prev, next])}
          />
        ))}
      </CardContent>
    </Card>
  )
}

function DeviceDetailPage() {
  const params = Route.useParams()
  const deviceId = params['device-id']
  const refetchInterval = usePollInterval(10_000)
  const detail = useDetail(deviceId, { query: { refetchInterval } })
  const me = useMe()

  const device = detail.data?.status === 200 ? detail.data.data : undefined
  const role = me.data?.status === 200 ? me.data.data.effectiveRole : undefined
  const operator = role === 'admin' || role === 'operator'

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/devices">
            <ArrowLeft className="size-4" />
            Devices
          </Link>
        </Button>
        {device && (
          <>
            <h1 className="text-xl font-semibold tracking-tight">
              {device.displayName ?? device.hostname}
            </h1>
            <PresenceBadge presence={device.presence} />
            {device.pinned && (
              <Badge variant="outline" className="gap-1 font-normal">
                <Pin className="size-3" /> pinned
              </Badge>
            )}
            {device.stale && (
              <Badge
                variant="outline"
                className="font-normal text-muted-foreground"
              >
                stale identity
              </Badge>
            )}
            <div className="ml-auto flex items-center gap-2">
              {operator && (
                <>
                  <Button variant="outline" size="sm" asChild>
                    <Link to="/deploy">
                      <Rocket className="size-4" />
                      Deploy
                    </Link>
                  </Button>
                  <Button size="sm" asChild>
                    <Link
                      to="/devices/$device-id/edit"
                      params={{ 'device-id': deviceId }}
                    >
                      <Pencil className="size-4" />
                      Edit
                    </Link>
                  </Button>
                </>
              )}
            </div>
          </>
        )}
      </div>

      {detail.data && detail.data.status === 404 ? (
        <p className="text-sm text-destructive">Unknown device.</p>
      ) : !device ? (
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : (
        <Tabs defaultValue="overview">
          <TabsList>
            <TabsTrigger value="overview">Overview</TabsTrigger>
            <TabsTrigger value="journal">Journal</TabsTrigger>
            <TabsTrigger value="terminal">Terminal</TabsTrigger>
          </TabsList>
          <TabsContent value="overview">
            <OverviewTab device={device} />
          </TabsContent>
          <TabsContent value="journal">
            <JournalTab deviceId={deviceId} />
          </TabsContent>
          <TabsContent value="terminal">
            <Card>
              <CardHeader>
                <CardTitle className="text-base">Remote terminal</CardTitle>
                <CardDescription>
                  Disabled by default; enabled only via a configuration change.
                  Sessions are short-lived and audited.
                </CardDescription>
              </CardHeader>
              <CardContent>
                <DeviceTerminal
                  deviceId={deviceId}
                  online={device.presence.state === 'online'}
                  operator={operator}
                />
              </CardContent>
            </Card>
          </TabsContent>
        </Tabs>
      )}
    </div>
  )
}
