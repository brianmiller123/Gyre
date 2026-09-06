/**
 * 前端消息目录：每种语言一个扁平 dotted-key 字典，一个语言一个文件。
 *
 * 拆分 + 懒加载：en（默认回退字典）内嵌主包保证零延迟回退；zh/ru/ja 通过
 * `loadLocaleDict` 动态 `import()` 按需加载（与 highlight.js 语言包同模式），
 * 不再让四语言全量常驻主 bundle。
 *
 * 新增语言：新建 `xx.ts`（`export const xx: Dict = { ... }`），在下方
 * `LocaleCode`/`SUPPORTED_LOCALES`/`loadLocaleDict` 各登记一行即可。
 */

import { en } from './en'

export type LocaleCode = 'en' | 'zh' | 'ru' | 'ja'

export type Dict = Record<string, string>

export { en }

export const SUPPORTED_LOCALES: LocaleCode[] = ['en', 'zh', 'ru', 'ja']

export const DEFAULT_LOCALE: LocaleCode = 'en'

/** 按需加载语言字典；en 直接返回内嵌对象（同步路径，永不必等待）。 */
export function loadLocaleDict(code: LocaleCode): Promise<Dict> | Dict {
  switch (code) {
    case 'zh':
      return import('./zh').then((m) => m.zh)
    case 'ru':
      return import('./ru').then((m) => m.ru)
    case 'ja':
      return import('./ja').then((m) => m.ja)
    case 'en':
      return en
  }
}
