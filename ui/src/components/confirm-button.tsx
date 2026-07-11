import { Button } from '@/components/ui/button'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog'

/**
 * Confirm gate for a destructive action (delete / revoke / abort).
 * A confirm/alert dialog — NOT a CRUD modal (CLAUDE.md forbids
 * create/edit/detail modals; a destructive-action confirm is the
 * allowed exception). Preserves the prior ConfirmButton API so call
 * sites are unchanged.
 */
export function ConfirmButton({
  label,
  confirmLabel = 'Confirm?',
  description,
  onConfirm,
  disabled,
  size = 'sm',
}: {
  label: string
  confirmLabel?: string
  description?: string
  onConfirm: () => void
  disabled?: boolean
  size?: 'sm' | 'default'
}) {
  return (
    <AlertDialog>
      <AlertDialogTrigger asChild>
        <Button variant="outline" size={size} disabled={disabled}>
          {label}
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{confirmLabel}</AlertDialogTitle>
          <AlertDialogDescription>
            {description ?? 'This action cannot be undone.'}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction variant="destructive" onClick={() => onConfirm()}>
            {label}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
