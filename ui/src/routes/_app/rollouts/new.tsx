import { useMemo, useState } from 'react'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, Plus, X } from 'lucide-react'
import { useList } from '@/api/endpoints/devices/devices'
import {
  getListRolloutsQueryKey,
  useCreateRoute,
} from '@/api/endpoints/rollouts/rollouts'
import { useListRevisions } from '@/api/endpoints/tree/tree'
import type { CohortSpec } from '@/api/model'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'

export const Route = createFileRoute('/_app/rollouts/new')({
  component: RolloutCreatePage,
})

/**
 * Create a rollout (spec/reeve/09-rollouts.md §11.1): pick the target
 * local revision, build the cohort (explicit devices, layer subtrees,
 * labels — selectors union; D12: labels group, never configure), and
 * set wave/gate parameters. Everything but revision+cohort is optional
 * with server defaults.
 */
function RolloutCreatePage() {
  const navigate = useNavigate()
  const qc = useQueryClient()
  const create = useCreateRoute()

  const revisions = useListRevisions({ limit: 100 })
  const localRevs = useMemo(
    () =>
      revisions.data?.status === 200
        ? revisions.data.data.filter((r) => r.stream === 'local')
        : [],
    [revisions.data],
  )
  const devices = useList()
  const allDevices = devices.data?.status === 200 ? devices.data.data : []

  const [revision, setRevision] = useState('')
  const [deviceFilter, setDeviceFilter] = useState('')
  const [pickedDevices, setPickedDevices] = useState<string[]>([])
  const [layers, setLayers] = useState<string[]>([])
  const [layerInput, setLayerInput] = useState('')
  const [labels, setLabels] = useState<Record<string, string>>({})
  const [labelKey, setLabelKey] = useState('')
  const [labelValue, setLabelValue] = useState('')
  const [waveCount, setWaveCount] = useState('')
  const [soakSecs, setSoakSecs] = useState('')
  const [passFraction, setPassFraction] = useState('')
  const [gateTimeoutSecs, setGateTimeoutSecs] = useState('')
  const [undeterminedAllowance, setUndeterminedAllowance] = useState('')
  const [failureThreshold, setFailureThreshold] = useState('')
  const [error, setError] = useState<string | null>(null)

  const cohortEmpty =
    pickedDevices.length === 0 && layers.length === 0 && Object.keys(labels).length === 0

  const filteredDevices = allDevices.filter((d) => {
    const needle = deviceFilter.toLowerCase()
    return (
      !needle ||
      d.hostname.toLowerCase().includes(needle) ||
      d.deviceId.toLowerCase().includes(needle)
    )
  })

  const toggleDevice = (id: string) =>
    setPickedDevices((prev) =>
      prev.includes(id) ? prev.filter((d) => d !== id) : [...prev, id],
    )

  const num = (s: string): number | null => (s.trim() === '' ? null : Number(s))

  const submit = async () => {
    setError(null)
    const cohort: CohortSpec = {}
    if (pickedDevices.length > 0) cohort.devices = pickedDevices
    if (layers.length > 0) cohort.layers = layers
    if (Object.keys(labels).length > 0) cohort.labels = labels

    const gate = {
      soakSecs: num(soakSecs),
      passFraction: passFraction.trim() === '' ? null : Number(passFraction),
      gateTimeoutSecs: num(gateTimeoutSecs),
      undeterminedAllowance: num(undeterminedAllowance),
    }
    const anyGate = Object.values(gate).some((v) => v != null)

    const res = await create.mutateAsync({
      data: {
        revision: Number(revision),
        cohort,
        waveCount: num(waveCount),
        failureThreshold: num(failureThreshold),
        ...(anyGate ? { gate } : {}),
      },
    })
    if (res.status === 201) {
      void qc.invalidateQueries({ queryKey: getListRolloutsQueryKey() })
      void navigate({
        to: '/rollouts/$rollout-id',
        params: { 'rollout-id': res.data.rolloutId },
      })
    } else {
      const detail =
        (res.status === 422 || res.status === 409) &&
        res.data &&
        typeof res.data === 'object' &&
        'error' in res.data
          ? String((res.data as { error: unknown }).error)
          : `HTTP ${res.status}`
      setError(detail)
    }
  }

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/rollouts">
            <ArrowLeft className="size-4" />
            Rollouts
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">New rollout</h1>
      </div>

      <div className="flex max-w-3xl flex-col gap-4">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Target revision</CardTitle>
            <CardDescription>
              The local tree revision this rollout advances the cohort to.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Select value={revision} onValueChange={setRevision}>
              <SelectTrigger className="max-w-xl">
                <SelectValue placeholder="Select a revision…" />
              </SelectTrigger>
              <SelectContent>
                {localRevs.map((r) => (
                  <SelectItem key={r.id} value={String(r.id)}>
                    r{r.id} — {r.message} ({r.author})
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Cohort</CardTitle>
            <CardDescription>
              Selectors union. Explicit devices, layer subtrees (D11 layer
              names), and label matches (all pairs must match).
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-5">
            <div className="flex flex-col gap-2">
              <Label>Devices ({pickedDevices.length} selected)</Label>
              <Input
                placeholder="Filter devices…"
                value={deviceFilter}
                onChange={(e) => setDeviceFilter(e.target.value)}
                className="max-w-72"
              />
              <div className="max-h-56 overflow-y-auto rounded-md border p-1">
                {filteredDevices.length === 0 ? (
                  <p className="px-2 py-1.5 text-sm text-muted-foreground">
                    {devices.isLoading ? 'Loading…' : 'No devices.'}
                  </p>
                ) : (
                  filteredDevices.map((d) => (
                    <label
                      key={d.deviceId}
                      className="flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-sm hover:bg-accent"
                    >
                      <input
                        type="checkbox"
                        checked={pickedDevices.includes(d.deviceId)}
                        onChange={() => toggleDevice(d.deviceId)}
                        className="accent-primary"
                      />
                      <span>{d.hostname}</span>
                      <span className="font-mono text-xs text-muted-foreground">
                        {d.deviceId}
                      </span>
                    </label>
                  ))
                )}
              </div>
            </div>

            <div className="flex flex-col gap-2">
              <Label htmlFor="layer-input">Layer subtrees</Label>
              <div className="flex flex-wrap gap-1">
                {layers.map((l) => (
                  <Badge key={l} variant="secondary" className="gap-1 font-mono font-normal">
                    {l}
                    <button
                      type="button"
                      onClick={() => setLayers((prev) => prev.filter((x) => x !== l))}
                      aria-label={`Remove ${l}`}
                    >
                      <X className="size-3" />
                    </button>
                  </Badge>
                ))}
              </div>
              <form
                className="flex items-center gap-2"
                onSubmit={(e) => {
                  e.preventDefault()
                  const v = layerInput.trim()
                  if (v && !layers.includes(v)) setLayers((prev) => [...prev, v])
                  setLayerInput('')
                }}
              >
                <Input
                  id="layer-input"
                  placeholder="site.plant-a or 20-site.plant-a"
                  value={layerInput}
                  onChange={(e) => setLayerInput(e.target.value)}
                  className="max-w-72 font-mono"
                />
                <Button type="submit" variant="outline" size="sm" disabled={!layerInput.trim()}>
                  <Plus className="size-4" />
                </Button>
              </form>
            </div>

            <div className="flex flex-col gap-2">
              <Label htmlFor="label-key">Labels (all must match)</Label>
              <div className="flex flex-wrap gap-1">
                {Object.entries(labels).map(([k, v]) => (
                  <Badge key={k} variant="secondary" className="gap-1 font-mono font-normal">
                    {k}={v}
                    <button
                      type="button"
                      onClick={() =>
                        setLabels((prev) => {
                          const next = { ...prev }
                          delete next[k]
                          return next
                        })
                      }
                      aria-label={`Remove ${k}`}
                    >
                      <X className="size-3" />
                    </button>
                  </Badge>
                ))}
              </div>
              <form
                className="flex items-center gap-2"
                onSubmit={(e) => {
                  e.preventDefault()
                  const k = labelKey.trim()
                  if (k) setLabels((prev) => ({ ...prev, [k]: labelValue.trim() }))
                  setLabelKey('')
                  setLabelValue('')
                }}
              >
                <Input
                  id="label-key"
                  placeholder="key"
                  value={labelKey}
                  onChange={(e) => setLabelKey(e.target.value)}
                  className="max-w-40 font-mono"
                />
                <Input
                  placeholder="value"
                  value={labelValue}
                  onChange={(e) => setLabelValue(e.target.value)}
                  className="max-w-40 font-mono"
                />
                <Button type="submit" variant="outline" size="sm" disabled={!labelKey.trim()}>
                  <Plus className="size-4" />
                </Button>
              </form>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Waves &amp; gate</CardTitle>
            <CardDescription>
              Blank fields use server defaults (§11.2/§11.3). No wave count =
              one wave covering the whole cohort.
            </CardDescription>
          </CardHeader>
          <CardContent className="grid grid-cols-2 gap-4 md:grid-cols-3">
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="wave-count">Wave count</Label>
              <Input
                id="wave-count"
                type="number"
                min={1}
                value={waveCount}
                onChange={(e) => setWaveCount(e.target.value)}
                placeholder="1"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="soak">Soak (seconds)</Label>
              <Input
                id="soak"
                type="number"
                min={0}
                value={soakSecs}
                onChange={(e) => setSoakSecs(e.target.value)}
                placeholder="default"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="pass-fraction">Pass fraction (0–1)</Label>
              <Input
                id="pass-fraction"
                type="number"
                min={0}
                max={1}
                step="0.05"
                value={passFraction}
                onChange={(e) => setPassFraction(e.target.value)}
                placeholder="default"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="gate-timeout">Gate timeout (seconds)</Label>
              <Input
                id="gate-timeout"
                type="number"
                min={0}
                value={gateTimeoutSecs}
                onChange={(e) => setGateTimeoutSecs(e.target.value)}
                placeholder="default"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="undetermined">Undetermined allowance</Label>
              <Input
                id="undetermined"
                type="number"
                min={0}
                value={undeterminedAllowance}
                onChange={(e) => setUndeterminedAllowance(e.target.value)}
                placeholder="unlimited (offline-first)"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="failure-threshold">Failure threshold</Label>
              <Input
                id="failure-threshold"
                type="number"
                min={0}
                value={failureThreshold}
                onChange={(e) => setFailureThreshold(e.target.value)}
                placeholder="default"
              />
            </div>
          </CardContent>
        </Card>

        <div className="flex items-center gap-3">
          <Button
            onClick={() => void submit()}
            disabled={!revision || cohortEmpty || create.isPending}
          >
            {create.isPending ? 'Creating…' : 'Create rollout'}
          </Button>
          {cohortEmpty && (
            <span className="text-xs text-muted-foreground">
              Pick at least one device, layer or label.
            </span>
          )}
          {error && <span className="text-sm text-destructive">{error}</span>}
        </div>
      </div>
    </div>
  )
}
