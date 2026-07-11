import { useMemo, useState } from 'react'
import { createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { parse as parseYaml } from 'yaml'
import { Rocket } from 'lucide-react'
import { useMe } from '@/api/endpoints/auth/auth'
import { useList, getListQueryKey } from '@/api/endpoints/devices/devices'
import { useDeploy, useUndeploy } from '@/api/endpoints/deploy/deploy'
import { getHistoryListQueryKey } from '@/api/endpoints/history/history'
import type { DeviceSummary, Scope, StackRef } from '@/api/model'
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
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs'
import { ScopePicker } from '@/components/scope-picker'
import { DeploymentStateBadge } from '@/components/deployment-state-badge'
import { devicesInScope, deviceLabel, scopeLabel } from '@/lib/scope'
import { groupPackages, useFileContent, useHeadFiles } from '@/lib/tree'
import { usePollInterval } from '@/lib/sse'

export const Route = createFileRoute('/_app/deploy/')({
  component: DeployPage,
})

/** Deployment profile ids declared in a package's margo.yaml (best-effort). */
function usePackageProfiles(name: string, version: string): string[] {
  const { files, streamOf, local, upstream } = useHeadFiles()
  const path = name && version ? `packages/${name}/${version}/margo.yaml` : ''
  const has = !!path && files != null && path in files
  const rev = has
    ? streamOf(path) === 'local'
      ? local?.id
      : upstream?.id
    : undefined
  const content = useFileContent(rev, has ? path : undefined)
  return useMemo(() => {
    if (!content.data?.text) return []
    try {
      const doc = parseYaml(content.data.text) as {
        deploymentProfiles?: { id?: string }[]
      }
      return (doc?.deploymentProfiles ?? [])
        .map((p) => p.id)
        .filter((id): id is string => !!id)
    } catch {
      return []
    }
  }, [content.data?.text])
}

/** What is currently running across the fleet, grouped by app. */
function FleetDeployments({ devices }: { devices: DeviceSummary[] }) {
  const apps = new Map<string, Map<string, number>>()
  for (const d of devices) {
    for (const dep of d.deployments) {
      const states = apps.get(dep.deploymentId) ?? new Map<string, number>()
      states.set(dep.state, (states.get(dep.state) ?? 0) + 1)
      apps.set(dep.deploymentId, states)
    }
  }
  const rows = [...apps.entries()].sort(([a], [b]) => a.localeCompare(b))
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Running across the fleet</CardTitle>
        <CardDescription>
          Apps devices currently report running. To remove one, choose it as
          the stack above, switch to Remove, and pick where to remove it from.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-2">
        {rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            Nothing deployed yet.
          </p>
        ) : (
          rows.map(([app, states]) => (
            <div key={app} className="flex items-center gap-3 text-sm">
              <span className="font-medium">{app}</span>
              <span className="flex flex-wrap gap-1">
                {[...states.entries()].map(([state, n]) => (
                  <span key={state} className="flex items-center gap-1">
                    <DeploymentStateBadge state={state} />
                    {n > 1 && (
                      <span className="text-xs text-muted-foreground">×{n}</span>
                    )}
                  </span>
                ))}
              </span>
            </div>
          ))
        )}
      </CardContent>
    </Card>
  )
}

/**
 * Deploy an app to a scope (§11.4): pick a stack (vendored package +
 * version + optional profile) and where it goes, preview the devices it
 * hits, then ship it. Remove is the same call taking the app back out of
 * the scope. This is the primary way to ship config — the numbered-file
 * editor is reachable under Advanced.
 */
function DeployPage() {
  const qc = useQueryClient()
  const refetchInterval = usePollInterval(30_000)
  const me = useMe()
  const role = me.data?.status === 200 ? me.data.data.effectiveRole : undefined
  const operator = role === 'admin' || role === 'operator'

  const devicesQ = useList({ query: { refetchInterval } })
  const devices = useMemo(
    () => (devicesQ.data?.status === 200 ? devicesQ.data.data : []),
    [devicesQ.data],
  )

  const { files } = useHeadFiles()
  const catalog = useMemo(() => {
    const byName = new Map<string, string[]>()
    if (files) {
      for (const p of groupPackages(files).values()) {
        const versions = byName.get(p.name) ?? []
        if (!versions.includes(p.version)) versions.push(p.version)
        byName.set(p.name, versions.sort())
      }
    }
    return byName
  }, [files])
  const packageNames = [...catalog.keys()].sort()

  const [mode, setMode] = useState<'deploy' | 'remove'>('deploy')
  const [pkg, setPkg] = useState('')
  const [version, setVersion] = useState('')
  const [profile, setProfile] = useState('')
  const [appName, setAppName] = useState('')
  const [scope, setScope] = useState<Scope | null>({ kind: 'all' })
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<string | null>(null)

  const versions = catalog.get(pkg) ?? []
  const profiles = usePackageProfiles(pkg, version)

  const deploy = useDeploy()
  const undeploy = useUndeploy()
  const pending = deploy.isPending || undeploy.isPending

  const preview = scope ? devicesInScope(devices, scope) : []
  const canSubmit = operator && !!pkg && !!version && !!scope && !pending

  const submit = async () => {
    if (!scope || !pkg || !version) return
    setError(null)
    setResult(null)
    const stack: StackRef = {
      package: pkg,
      version,
      ...(profile.trim() ? { profile: profile.trim() } : {}),
      ...(appName.trim() ? { name: appName.trim() } : {}),
    }
    const call = mode === 'deploy' ? deploy : undeploy
    const res = await call.mutateAsync({ data: { stack, scope } })
    if (res.status === 200) {
      const verb = mode === 'deploy' ? 'Deployed' : 'Removed'
      setResult(
        res.data.changed
          ? `${verb} ${res.data.app} — ${res.data.scope}.`
          : `No change — ${res.data.app} was already ${mode === 'deploy' ? 'deployed to' : 'absent from'} ${res.data.scope}.`,
      )
      void qc.invalidateQueries({ queryKey: getListQueryKey() })
      void qc.invalidateQueries({ queryKey: getHistoryListQueryKey() })
    } else {
      const body = res.data
      setError(
        body && typeof body === 'object' && 'error' in body
          ? String((body as { error: unknown }).error)
          : `HTTP ${res.status}`,
      )
    }
  }

  return (
    <div className="flex flex-col gap-4 p-6">
      <h1 className="text-xl font-semibold tracking-tight">Deploy</h1>

      {!operator && (
        <p className="text-sm text-muted-foreground">
          Deploying requires the operator role.
        </p>
      )}

      <div className="flex max-w-3xl flex-col gap-4">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Stack</CardTitle>
            <CardDescription>
              The app to ship, from your uploaded packages.
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-4">
            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
              <div className="flex flex-col gap-1.5">
                <Label>Package</Label>
                <Select
                  value={pkg}
                  onValueChange={(v) => {
                    setPkg(v)
                    setVersion('')
                    setProfile('')
                  }}
                >
                  <SelectTrigger>
                    <SelectValue placeholder="Choose a package…" />
                  </SelectTrigger>
                  <SelectContent>
                    {packageNames.length === 0 ? (
                      <SelectItem value="__none__" disabled>
                        No packages uploaded yet
                      </SelectItem>
                    ) : (
                      packageNames.map((n) => (
                        <SelectItem key={n} value={n}>
                          {n}
                        </SelectItem>
                      ))
                    )}
                  </SelectContent>
                </Select>
              </div>
              <div className="flex flex-col gap-1.5">
                <Label>Version</Label>
                <Select
                  value={version}
                  onValueChange={setVersion}
                  disabled={!pkg}
                >
                  <SelectTrigger>
                    <SelectValue placeholder="Choose a version…" />
                  </SelectTrigger>
                  <SelectContent>
                    {versions.map((v) => (
                      <SelectItem key={v} value={v}>
                        {v}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            </div>
            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="profile">Profile (optional)</Label>
                <Input
                  id="profile"
                  list="profile-options"
                  placeholder="package default"
                  value={profile}
                  onChange={(e) => setProfile(e.target.value)}
                />
                <datalist id="profile-options">
                  {profiles.map((p) => (
                    <option key={p} value={p} />
                  ))}
                </datalist>
              </div>
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="app-name">App name (optional)</Label>
                <Input
                  id="app-name"
                  placeholder={pkg || 'defaults to package'}
                  value={appName}
                  onChange={(e) => setAppName(e.target.value)}
                />
              </div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Where</CardTitle>
            <CardDescription>
              Deploy to the whole fleet, one group, or a hand-picked set.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <ScopePicker
              devices={devices}
              loading={devicesQ.isLoading}
              onChange={setScope}
            />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Review</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-3">
            <Tabs value={mode} onValueChange={(v) => setMode(v as typeof mode)}>
              <TabsList>
                <TabsTrigger value="deploy">Deploy</TabsTrigger>
                <TabsTrigger value="remove">Remove</TabsTrigger>
              </TabsList>
            </Tabs>

            <p className="text-sm">
              {pkg && version && scope ? (
                <>
                  {mode === 'deploy' ? 'Deploy ' : 'Remove '}
                  <span className="font-medium">
                    {appName.trim() || pkg} {version}
                  </span>{' '}
                  {mode === 'deploy' ? 'to' : 'from'}{' '}
                  <span className="font-medium">{scopeLabel(scope)}</span>.
                </>
              ) : (
                <span className="text-muted-foreground">
                  Choose a package, version, and where it goes.
                </span>
              )}
            </p>

            {scope && (
              <div className="flex flex-col gap-1.5">
                <span className="text-xs text-muted-foreground">
                  Affects {preview.length} device
                  {preview.length === 1 ? '' : 's'} right now
                </span>
                {preview.length > 0 && (
                  <div className="flex flex-wrap gap-1">
                    {preview.slice(0, 24).map((d) => (
                      <Badge
                        key={d.deviceId}
                        variant="secondary"
                        className="font-normal"
                      >
                        {deviceLabel(d)}
                      </Badge>
                    ))}
                    {preview.length > 24 && (
                      <Badge variant="outline" className="font-normal">
                        +{preview.length - 24} more
                      </Badge>
                    )}
                  </div>
                )}
              </div>
            )}

            <div className="flex items-center gap-3">
              <Button onClick={() => void submit()} disabled={!canSubmit}>
                <Rocket className="size-4" />
                {pending
                  ? 'Working…'
                  : mode === 'deploy'
                    ? 'Deploy'
                    : 'Remove'}
              </Button>
              {result && (
                <span className="text-sm text-emerald-600 dark:text-emerald-400">
                  {result}
                </span>
              )}
              {error && <span className="text-sm text-destructive">{error}</span>}
            </div>
          </CardContent>
        </Card>

        <FleetDeployments devices={devices} />
      </div>
    </div>
  )
}
