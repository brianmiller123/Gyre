/**
 * Domain model for the C5 console.
 * These shapes are shared by the mock data layer, tables and forms so the
 * whole UI stays type-safe and consistent.
 */

export type ID = string

export type Provider = 'aws' | 'gcp' | 'azure' | 'onprem'
export type Health = 'healthy' | 'degraded' | 'down'
export type RunStatus = 'running' | 'pending' | 'failed' | 'stopped'
export type DeployState = 'success' | 'running' | 'failed' | 'cancelled'
export type Severity = 'info' | 'success' | 'warning' | 'danger'
export type Environment = 'production' | 'staging' | 'development'

export interface Cluster {
  id: ID
  name: string
  region: string
  provider: Provider
  version: string
  nodes: number
  cpu: number // percent 0–100
  memory: number // percent 0–100
  pods: number
  podCapacity: number
  status: RunStatus
  health: Health
  cost: number // monthly USD
  owner: string
  updatedAt: string // ISO
}

export interface Service {
  id: ID
  name: string
  cluster: string
  replicas: number
  ready: number
  latencyP95: number // ms
  rps: number
  errorRate: number // percent
  status: RunStatus
  owner: string
  image: string
  language: string
}

export interface Deployment {
  id: ID
  service: string
  version: string
  environment: Environment
  state: DeployState
  author: string
  commit: string
  message: string
  durationSec: number
  startedAt: string // ISO
}

export type Role = 'Owner' | 'Admin' | 'Developer' | 'Viewer'
export type MemberStatus = 'active' | 'invited' | 'suspended'

export interface Member {
  id: ID
  name: string
  email: string
  role: Role
  status: MemberStatus
  lastSeen: string // ISO
  hue: number // avatar color hue
  teams: string[]
  twoFactor: boolean
}

export type NotificationKind = 'deploy' | 'alert' | 'security' | 'system' | 'invite'

export interface AppNotification {
  id: ID
  kind: NotificationKind
  title: string
  body: string
  time: string // ISO
  read: boolean
  severity: Severity
}

/** A point in a time series. */
export interface SeriesPoint {
  label: string
  value: number
  value2?: number
}
