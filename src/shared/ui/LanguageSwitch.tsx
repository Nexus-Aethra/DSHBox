import type { Language } from '../types/domain'

export function LanguageSwitch({ language, onChange }: { language: Language; onChange: (language: Language) => Promise<void> }) {
  return <div className="language-switch"><button type="button" className={language === 'en' ? 'active' : ''} onClick={() => { void onChange('en') }}>EN</button><button type="button" className={language === 'zh-CN' ? 'active' : ''} onClick={() => { void onChange('zh-CN') }}>中文</button></div>
}
