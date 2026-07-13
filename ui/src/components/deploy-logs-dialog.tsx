import { useEffect, useState } from 'react'
import { FileText, ScrollText } from 'lucide-react'
import {
  useGetDeployLog,
  useListDeployLogs,
} from '@/api/endpoints/logs/logs'
import type { DeployLogMeta, DeployLogOutcome } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog'
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import { ScrollArea } from '@/components/ui/scroll-area'
import { cn } from '@/lib/utils'
import { fmtRfc3339 } from '@/lib/format'

/** Byte count -> compact human string ("812 B", "3.4 KB"). */
function fmtBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  return `${(bytes / 1024).toFixed(1)} KB`
}

/** Outcome pill — mirrors the deploy state badge tones. */
function OutcomeBadge({ outcome }: { outcome: DeployLogOutcome }) {
  const tone =
    outcome === 'applied'
      ? 'border-emerald-500/40 text-emerald-600 dark:text-emerald-400'
      : outcome === 'failed'
        ? 'border-red-500/40 text-red-600 dark:text-red-400'
        : 'text-muted-foreground'
  return (
    <Badge variant="outline" className={cn('font-normal', tone)}>
      {outcome}
    </Badge>
  )
}

/** One selectable row in the log list. */
function LogListItem({
  log,
  selected,
  onSelect,
}: {
  log: DeployLogMeta
  selected: boolean
  onSelect: () => void
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      className={cn(
        'flex w-full flex-col gap-1 border-b px-3 py-2 text-left last:border-b-0 hover:bg-muted/50',
        selected && 'bg-muted',
      )}
    >
      <div className="flex items-center gap-2">
        <OutcomeBadge outcome={log.outcome} />
        <span className="font-mono text-xs text-muted-foreground">
          compose {log.phase}
        </span>
      </div>
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <span>{fmtRfc3339(log.capturedAt)}</span>
        <span>·</span>
        <span>{fmtBytes(log.sizeBytes)}</span>
        {log.truncated && (
          <Badge
            variant="outline"
            className="font-normal text-muted-foreground"
          >
            truncated
          </Badge>
        )}
      </div>
    </button>
  )
}

/** The selected log's captured text in a monospace scroll block. */
function LogText({ deviceId, logId }: { deviceId: string; logId: string }) {
  const content = useGetDeployLog(deviceId, logId)
  if (content.isLoading)
    return (
      <p className="p-4 text-sm text-muted-foreground">Loading output…</p>
    )
  if (!content.data || content.data.status !== 200)
    return (
      <p className="p-4 text-sm text-destructive">Could not load this log.</p>
    )
  const { meta, text } = content.data.data
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {meta.truncated && (
        <p className="border-b bg-muted/40 px-4 py-1.5 text-xs text-muted-foreground">
          Output was clipped by the agent before upload; the tail may be
          missing.
        </p>
      )}
      <ScrollArea className="min-h-0 flex-1">
        <pre className="p-4 font-mono text-xs whitespace-pre-wrap break-words">
          {text.length === 0 ? '(no output captured)' : text}
        </pre>
      </ScrollArea>
    </div>
  )
}

function LogsBody({
  deviceId,
  deploymentId,
  open,
}: {
  deviceId: string
  deploymentId: string
  open: boolean
}) {
  const list = useListDeployLogs(
    deviceId,
    { deployment: deploymentId },
    { query: { enabled: open } },
  )
  const [selectedId, setSelectedId] = useState<string | null>(null)

  const logs =
    list.data?.status === 200 ? list.data.data.logs : ([] as DeployLogMeta[])

  // Default to the newest log; if the selection drops out of the list
  // (e.g. pruned by retention), fall back to the newest again.
  useEffect(() => {
    if (logs.length === 0) {
      if (selectedId !== null) setSelectedId(null)
      return
    }
    if (!selectedId || !logs.some((l) => l.id === selectedId))
      setSelectedId(logs[0].id)
  }, [logs, selectedId])

  if (list.isLoading)
    return (
      <p className="p-6 text-sm text-muted-foreground">Loading logs…</p>
    )

  if (logs.length === 0)
    return (
      <Empty className="border">
        <EmptyHeader>
          <EmptyMedia variant="icon">
            <ScrollText />
          </EmptyMedia>
          <EmptyTitle>No logs captured</EmptyTitle>
          <EmptyDescription>
            The device has not sent any deploy output for this deployment
            yet. Logs appear here the next time it applies or removes the
            deployment.
          </EmptyDescription>
        </EmptyHeader>
      </Empty>
    )

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-md border md:flex-row">
      <ScrollArea className="max-h-40 shrink-0 border-b md:max-h-none md:w-64 md:border-r md:border-b-0">
        <div className="flex flex-col">
          {logs.map((log) => (
            <LogListItem
              key={log.id}
              log={log}
              selected={log.id === selectedId}
              onSelect={() => setSelectedId(log.id)}
            />
          ))}
        </div>
      </ScrollArea>
      <div className="flex min-h-0 flex-1 flex-col">
        {selectedId ? (
          <LogText deviceId={deviceId} logId={selectedId} />
        ) : (
          <p className="p-4 text-sm text-muted-foreground">
            Select a log to view its output.
          </p>
        )}
      </div>
    </div>
  )
}

/**
 * Read-only viewer for the deploy output (`docker compose up`/`down`)
 * a device captured for one deployment. Opens on demand; the list and
 * bodies are fetched only while the dialog is open.
 */
export function DeployLogsDialog({
  deviceId,
  deploymentId,
}: {
  deviceId: string
  deploymentId: string
}) {
  const [open, setOpen] = useState(false)
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          <FileText className="size-4" />
          Logs
        </Button>
      </DialogTrigger>
      <DialogContent className="flex max-h-[85vh] flex-col gap-4 sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>Deploy logs</DialogTitle>
          <DialogDescription>
            Captured <span className="font-mono">docker compose</span> output
            for deployment{' '}
            <span className="font-mono">{deploymentId}</span>. Newest first.
          </DialogDescription>
        </DialogHeader>
        <LogsBody deviceId={deviceId} deploymentId={deploymentId} open={open} />
      </DialogContent>
    </Dialog>
  )
}
