import { useEffect } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { Outlet, createFileRoute, redirect } from '@tanstack/react-router'
import { getMeQueryOptions } from '@/api/endpoints/auth/auth'
import { AppHeader } from '@/components/app-header'
import { AppSidebar } from '@/components/app-sidebar'
import { SidebarInset, SidebarProvider } from '@/components/ui/sidebar'
import { startSse } from '@/lib/sse'

export const Route = createFileRoute('/_app')({
  // Auth gate: /api/auth/me always answers 200; `effectiveRole` is the
  // mode-aware acting role (REEVE_AUTH=none -> anonymous acts as
  // admin, so none-mode skips login entirely; password mode without a
  // session yields anonymous with no role -> login).
  beforeLoad: async ({ context }) => {
    const res = await context.queryClient.ensureQueryData(getMeQueryOptions())
    if (res.status !== 200 || !res.data.effectiveRole) {
      throw redirect({ to: '/login' })
    }
  },
  component: AppLayout,
})

function AppLayout() {
  const queryClient = useQueryClient()

  // One live event stream for the whole authed app; SSE events map to
  // Query invalidations (src/lib/sse.ts, spec/reeve/04-status-stream.md §6).
  useEffect(() => startSse(queryClient), [queryClient])

  return (
    <SidebarProvider>
      <AppSidebar />
      <SidebarInset className="min-h-0">
        <AppHeader />
        <main className="min-h-0 flex-1 overflow-y-auto">
          <Outlet />
        </main>
      </SidebarInset>
    </SidebarProvider>
  )
}
