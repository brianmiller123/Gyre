/** Tiny classnames helper — keeps conditional class composition readable. */
export function cn(
  ...classes: Array<string | false | null | undefined>
): string {
  return classes.filter(Boolean).join(' ')
}
