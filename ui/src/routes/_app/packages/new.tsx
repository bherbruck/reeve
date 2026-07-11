import { useRef, useState } from 'react'
import { Link, createFileRoute, useNavigate } from '@tanstack/react-router'
import { useQueryClient } from '@tanstack/react-query'
import { ArrowLeft, FolderOpen } from 'lucide-react'
import { parse as parseYaml } from 'yaml'
import { usePutPackage } from '@/api/endpoints/tree/tree'
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
import { bytesToBase64, bytesToText } from '@/lib/base64'

export const Route = createFileRoute('/_app/packages/new')({
  component: PackageUploadPage,
})

interface PickedFile {
  rel: string
  base64: string
  size: number
}

const NAME_RE = /^[a-z0-9][a-z0-9.-]*$/

/**
 * Vendor an application package: pick a package directory, PUT its files
 * to /api/tree/packages/{name}/{version} (one declarative commit; the
 * server validates the package and returns warnings).
 */
function PackageUploadPage() {
  const navigate = useNavigate()
  const qc = useQueryClient()
  const put = usePutPackage()
  const inputRef = useRef<HTMLInputElement>(null)

  const [picked, setPicked] = useState<PickedFile[]>([])
  const [name, setName] = useState('')
  const [version, setVersion] = useState('')
  const [message, setMessage] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [warnings, setWarnings] = useState<string[]>([])

  const onPick = async (list: FileList | null) => {
    if (!list || list.length === 0) return
    setError(null)
    const files: PickedFile[] = []
    for (const file of Array.from(list)) {
      // webkitRelativePath = "<picked-dir>/<rel...>" — strip the root.
      const relParts = (file.webkitRelativePath || file.name).split('/')
      const rel = relParts.length > 1 ? relParts.slice(1).join('/') : relParts[0]
      const bytes = new Uint8Array(await file.arrayBuffer())
      files.push({ rel, base64: bytesToBase64(bytes), size: bytes.length })
    }
    files.sort((a, b) => a.rel.localeCompare(b.rel))
    setPicked(files)

    // Prefill name/version from the package's margo.yaml when present.
    const margo = files.find((f) => f.rel === 'margo.yaml')
    if (margo) {
      try {
        const text = bytesToText(
          Uint8Array.from(atob(margo.base64), (c) => c.charCodeAt(0)),
        )
        const doc: unknown = text != null ? parseYaml(text) : null
        if (typeof doc === 'object' && doc != null) {
          const d = doc as {
            id?: string
            metadata?: { id?: string; name?: string; version?: string }
          }
          const id = d.id ?? d.metadata?.id ?? d.metadata?.name
          if (id && !name) setName(String(id))
          if (d.metadata?.version && !version) setVersion(String(d.metadata.version))
        }
      } catch {
        // Prefill is best-effort; the server validates authoritatively.
      }
    }
  }

  const upload = async () => {
    setError(null)
    setWarnings([])
    const body: Record<string, string> = {}
    for (const f of picked) body[f.rel] = f.base64
    const res = await put.mutateAsync({
      name,
      version,
      data: { files: body, message: message.trim() || null },
    })
    if (res.status === 200) {
      setWarnings(res.data.warnings ?? [])
      void qc.invalidateQueries()
      if ((res.data.warnings ?? []).length === 0) {
        void navigate({
          to: '/packages/$name/$version',
          params: { name, version },
        })
      }
    } else {
      const detail =
        res.status === 422 && res.data && typeof res.data === 'object' && 'error' in res.data
          ? String((res.data as { error: unknown }).error)
          : `HTTP ${res.status}`
      setError(detail)
    }
  }

  const valid =
    picked.length > 0 && NAME_RE.test(name) && NAME_RE.test(version)

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/packages">
            <ArrowLeft className="size-4" />
            Packages
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">Upload package</h1>
      </div>

      <Card className="max-w-3xl">
        <CardHeader>
          <CardTitle className="text-base">Package directory</CardTitle>
          <CardDescription>
            Pick the directory that contains margo.yaml. The whole directory is
            uploaded as one package version, ready to deploy.
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <input
            ref={inputRef}
            type="file"
            // Non-standard but universal directory picker.
            {...{ webkitdirectory: '', directory: '' }}
            multiple
            className="hidden"
            onChange={(e) => void onPick(e.target.files)}
          />
          <div className="flex items-center gap-3">
            <Button variant="outline" onClick={() => inputRef.current?.click()}>
              <FolderOpen className="size-4" />
              Choose directory
            </Button>
            <span className="text-sm text-muted-foreground">
              {picked.length === 0
                ? 'Nothing picked yet.'
                : `${picked.length} file${picked.length === 1 ? '' : 's'} staged.`}
            </span>
          </div>

          {picked.length > 0 && (
            <pre className="max-h-48 overflow-auto rounded bg-muted p-2 font-mono text-xs">
              {picked.map((f) => `${f.rel} (${f.size} B)`).join('\n')}
            </pre>
          )}

          <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="pkg-name">Package name</Label>
              <Input
                id="pkg-name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="com-example-app"
                className="font-mono"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="pkg-version">Version</Label>
              <Input
                id="pkg-version"
                value={version}
                onChange={(e) => setVersion(e.target.value)}
                placeholder="1.0.0"
                className="font-mono"
              />
            </div>
          </div>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor="pkg-message">Commit message</Label>
            <Input
              id="pkg-message"
              value={message}
              onChange={(e) => setMessage(e.target.value)}
              placeholder={`vendor ${name || '<name>'} ${version || '<version>'}`}
            />
          </div>

          <div className="flex items-center gap-3">
            <Button onClick={() => void upload()} disabled={!valid || put.isPending}>
              {put.isPending ? 'Uploading…' : 'Commit package'}
            </Button>
            {error && <span className="text-sm text-destructive">{error}</span>}
          </div>

          {warnings.length > 0 && (
            <div className="rounded-md border border-amber-500/40 p-3">
              <p className="mb-1 text-sm font-medium text-amber-600 dark:text-amber-400">
                Committed with validation warnings:
              </p>
              <ul className="list-inside list-disc text-sm text-muted-foreground">
                {warnings.map((w) => (
                  <li key={w}>{w}</li>
                ))}
              </ul>
              <Button
                variant="outline"
                size="sm"
                className="mt-2"
                asChild
              >
                <Link
                  to="/packages/$name/$version"
                  params={{ name, version }}
                >
                  View package
                </Link>
              </Button>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
