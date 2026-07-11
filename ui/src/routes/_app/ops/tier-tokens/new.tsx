import { useState } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, Plus, X } from 'lucide-react'
import {
  getListTokensRouteQueryKey,
  useCreateTokenRoute,
} from '@/api/endpoints/federation/federation'
import type { CreatedTierToken } from '@/api/model'
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
import { CopyButton } from '@/components/copy-button'

export const Route = createFileRoute('/_app/ops/tier-tokens/new')({
  component: TierTokenCreatePage,
})

/**
 * Mint a child-tier sync token (admin only — the server enforces the
 * role). Raw token shown once.
 */
function TierTokenCreatePage() {
  const qc = useQueryClient()
  const create = useCreateTokenRoute()

  const [name, setName] = useState('')
  const [site, setSite] = useState('')
  const [prefixes, setPrefixes] = useState<string[]>([])
  const [prefixInput, setPrefixInput] = useState('')
  const [ttlHours, setTtlHours] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [minted, setMinted] = useState<CreatedTierToken | null>(null)

  const submit = async () => {
    setError(null)
    const res = await create.mutateAsync({
      data: {
        name: name.trim(),
        site: site.trim(),
        syncPrefixes: prefixes.length > 0 ? prefixes : null,
        ttlSecs:
          ttlHours.trim() === '' ? null : Math.round(Number(ttlHours) * 3600),
      },
    })
    if (res.status === 200) {
      setMinted(res.data)
      void qc.invalidateQueries({ queryKey: getListTokensRouteQueryKey() })
    } else {
      const detail =
        (res.status === 409 || res.status === 422) &&
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
          <Link to="/ops">
            <ArrowLeft className="size-4" />
            Ops
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">New tier token</h1>
      </div>

      {minted ? (
        <Card className="max-w-2xl border-emerald-500/40">
          <CardHeader>
            <CardTitle className="text-base">One-time tier token</CardTitle>
            <CardDescription>
              Shown exactly once — only the hash is stored. Configure the
              child tier ({minted.name}) with it as REEVE_UPSTREAM
              credentials.
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-3">
            <div className="flex items-center gap-2">
              <code className="break-all rounded bg-muted px-3 py-2 font-mono text-sm">
                {minted.token}
              </code>
              <CopyButton value={minted.token} />
            </div>
            <p className="text-sm text-muted-foreground">
              Site {minted.site} · syncs {minted.syncPrefixes.join(', ')}
            </p>
            <div>
              <Button variant="outline" size="sm" asChild>
                <Link to="/ops">Back to ops</Link>
              </Button>
            </div>
          </CardContent>
        </Card>
      ) : (
        <Card className="max-w-2xl">
          <CardHeader>
            <CardTitle className="text-base">Token parameters</CardTitle>
            <CardDescription>
              The child gateway syncs only content under its prefixes; blank
              prefixes use the server default set.
            </CardDescription>
          </CardHeader>
          <CardContent className="flex flex-col gap-4">
            <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="tier-name">Tier name</Label>
                <Input
                  id="tier-name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="gateway-plant-a"
                  className="font-mono"
                />
              </div>
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="tier-site">Site label</Label>
                <Input
                  id="tier-site"
                  value={site}
                  onChange={(e) => setSite(e.target.value)}
                  placeholder="plant-a"
                  className="font-mono"
                />
                <span className="text-xs text-muted-foreground">
                  The site this gateway serves.
                </span>
              </div>
            </div>

            <div className="flex flex-col gap-1.5">
              <Label htmlFor="prefix-input">Sync prefixes (optional)</Label>
              <div className="flex flex-wrap gap-1">
                {prefixes.map((p) => (
                  <Badge key={p} variant="secondary" className="gap-1 font-mono font-normal">
                    {p}
                    <button
                      type="button"
                      onClick={() =>
                        setPrefixes((prev) => prev.filter((x) => x !== p))
                      }
                      aria-label={`Remove ${p}`}
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
                  const v = prefixInput.trim()
                  if (v && !prefixes.includes(v))
                    setPrefixes((prev) => [...prev, v])
                  setPrefixInput('')
                }}
              >
                <Input
                  id="prefix-input"
                  value={prefixInput}
                  onChange={(e) => setPrefixInput(e.target.value)}
                  placeholder="packages/"
                  className="max-w-72 font-mono"
                />
                <Button
                  type="submit"
                  variant="outline"
                  size="sm"
                  disabled={!prefixInput.trim()}
                >
                  <Plus className="size-4" />
                </Button>
              </form>
            </div>

            <div className="flex max-w-48 flex-col gap-1.5">
              <Label htmlFor="tier-ttl">TTL (hours, blank = never)</Label>
              <Input
                id="tier-ttl"
                type="number"
                min={0}
                step="any"
                value={ttlHours}
                onChange={(e) => setTtlHours(e.target.value)}
              />
            </div>

            <div className="flex items-center gap-3">
              <Button
                onClick={() => void submit()}
                disabled={!name.trim() || !site.trim() || create.isPending}
              >
                {create.isPending ? 'Minting…' : 'Mint tier token'}
              </Button>
              {error && <span className="text-sm text-destructive">{error}</span>}
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  )
}
