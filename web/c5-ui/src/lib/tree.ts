/**
 * 树形缩进统一算法（分支树 / 工作区文件树共用，此前两处各写一套魔法数）。
 * 每层 14px、基础 6px——与分支树原值一致；文件树由 12/4 收敛到同一节奏。
 */
export function treeIndent(depth: number): number {
  return depth * 14 + 6
}
