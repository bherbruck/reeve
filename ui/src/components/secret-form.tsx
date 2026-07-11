import { useMemo, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { getListSecretsQueryKey, usePutRoute } from '@/api/endpoints/secrets/secrets'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'

const SCOPE_KINDS = ['fleet', 'class', 'region', 'site', 'device'] as const
type ScopeKind = (typeof SCOPE_KINDS)[number]

function splitScope(scope: string | undefined): { kind: ScopeKind; qualifier: string } {
  if (!scope || scope === 'fleet') return { kind: 'fleet', qualifier: '' }
  const dot = scope.indexOf('.')
  const kind = (dot === -1 ? scope : scope.slice(0, dot)) as ScopeKind
  if (!SCOPE_KINDS.includes(kind)) return { kind: 'fleet', qualifier: '' }
  return { kind, qualifier: dot === -1 ? '' : scope.slice(dot + 1) }
}

/**
 * Set/rotate a secret — the ONE write surface (§12.2 write-only: the
 * value is plaintext in this form's memory and the request body only;
 * it is never readable back, so this component never displays stored
 * values). Setting an existing (scope, name) IS rotation: version
 * bumps and secrets_version propagation re-ups exactly the consuming
 * services.
 *
 * Scope grammar (spec/reeve/10-secrets.md):
 * `fleet | class.<n> | region.<n> | site.<n> | device.<id>`.
 */
export function SecretForm({
  initialName = '',
  initialScope = 'fleet',
  onDone,
}: {
  initialName?: string
  initialScope?: string
  onDone?: (name: string, scope: string, version: number) => void
}) {
  const qc = useQueryClient()
  const put = usePutRoute()

  const initial = useMemo(() => splitScope(initialScope), [initialScope])
  const [name, setName] = useState(initialName)
  const [kind, setKind] = useState<ScopeKind>(initial.kind)
  const [qualifier, setQualifier] = useState(initial.qualifier)
  const [value, setValue] = useState('')
  const [showValue, setShowValue] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<string | null>(null)

  const scope = kind === 'fleet' ? 'fleet' : `${kind}.${qualifier.trim()}`
  const valid =
    name.trim().length > 0 &&
    value.length > 0 &&
    (kind === 'fleet' || qualifier.trim().length > 0)

  const submit = async () => {
    setError(null)
    setDone(null)
    const res = await put.mutateAsync({
      data: { name: name.trim(), scope, value },
    })
    // The plaintext leaves this component's state immediately either way.
    setValue('')
    setShowValue(false)
    if (res.status === 200) {
      void qc.invalidateQueries({ queryKey: getListSecretsQueryKey() })
      setDone(
        `Stored ${res.data.scope}/${res.data.name} at version ${res.data.version}. The value is not readable back.`,
      )
      onDone?.(res.data.name, res.data.scope, res.data.version)
    } else {
      const detail =
        res.status === 422 && res.data && typeof res.data === 'object' && 'error' in res.data
          ? String((res.data as { error: unknown }).error)
          : `HTTP ${res.status}`
      setError(detail)
    }
  }

  return (
    <div className="flex max-w-xl flex-col gap-4">
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="secret-name">Name</Label>
        <Input
          id="secret-name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="db-password"
          className="font-mono"
        />
      </div>

      <div className="flex flex-col gap-1.5">
        <Label>Scope</Label>
        <div className="flex items-center gap-2">
          <Select value={kind} onValueChange={(v) => setKind(v as ScopeKind)}>
            <SelectTrigger className="w-36">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {SCOPE_KINDS.map((k) => (
                <SelectItem key={k} value={k}>
                  {k}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {kind !== 'fleet' && (
            <Input
              value={qualifier}
              onChange={(e) => setQualifier(e.target.value)}
              placeholder={kind === 'device' ? '<device id>' : `<${kind} name>`}
              className="font-mono"
            />
          )}
        </div>
        <span className="font-mono text-xs text-muted-foreground">{scope}</span>
      </div>

      <div className="flex flex-col gap-1.5">
        <Label htmlFor="secret-value">Value (write-only)</Label>
        <div className="flex items-center gap-2">
          <Input
            id="secret-value"
            type={showValue ? 'text' : 'password'}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            autoComplete="off"
            className="font-mono"
          />
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => setShowValue((s) => !s)}
          >
            {showValue ? 'Hide' : 'Show'}
          </Button>
        </div>
        <span className="text-xs text-muted-foreground">
          Sealed before it touches the database; never displayed again after
          submit.
        </span>
      </div>

      <div className="flex items-center gap-3">
        <Button onClick={() => void submit()} disabled={!valid || put.isPending}>
          {put.isPending ? 'Storing…' : 'Store secret'}
        </Button>
        {error && <span className="text-sm text-destructive">{error}</span>}
      </div>
      {done && (
        <p className="text-sm text-emerald-600 dark:text-emerald-400">{done}</p>
      )}
    </div>
  )
}
