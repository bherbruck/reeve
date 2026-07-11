import { useQueryClient } from '@tanstack/react-query'
import { useNavigate } from '@tanstack/react-router'
import { LogOut } from 'lucide-react'
import { useLogout, useMe } from '@/api/endpoints/auth/auth'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { SidebarTrigger } from '@/components/ui/sidebar'
import { cn } from '@/lib/utils'
import { useSseConnected } from '@/lib/sse'

/** SSE freshness dot (§6.2: connect/disconnect is invisible beyond this). */
function StreamIndicator() {
  const up = useSseConnected()
  return (
    <span
      className="flex items-center gap-1.5 text-xs text-muted-foreground"
      title={
        up
          ? 'live event stream connected'
          : 'event stream down — polling fallback active'
      }
    >
      <span
        className={cn(
          'size-2 rounded-full',
          up ? 'bg-emerald-500' : 'bg-amber-500',
        )}
      />
      {up ? 'live' : 'polling'}
    </span>
  )
}

export function AppHeader() {
  const me = useMe()
  const queryClient = useQueryClient()
  const navigate = useNavigate()
  const logout = useLogout({
    mutation: {
      onSuccess: async () => {
        queryClient.clear()
        await navigate({ to: '/login' })
      },
    },
  })

  const who = me.data?.status === 200 ? me.data.data : undefined
  const label =
    who?.kind === 'human'
      ? (who.user ?? 'user')
      : who?.kind === 'anonymous'
        ? 'anonymous'
        : ''

  return (
    <header className="flex h-14 shrink-0 items-center gap-4 border-b px-6">
      <SidebarTrigger className="-ml-2" />
      <div className="flex-1" />
      <StreamIndicator />
      {who && (
        <span className="flex items-center gap-2 text-sm">
          {label}
          {who.effectiveRole && (
            <Badge variant="secondary" className="font-normal">
              {who.effectiveRole}
            </Badge>
          )}
        </span>
      )}
      {who?.kind === 'human' && (
        <Button
          variant="ghost"
          size="sm"
          onClick={() => logout.mutate()}
          disabled={logout.isPending}
        >
          <LogOut className="size-4" />
          Log out
        </Button>
      )}
    </header>
  )
}
