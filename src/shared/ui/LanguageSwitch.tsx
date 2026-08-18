import type { Language } from '../types/domain'
import { Button } from '../../ui/Button'

export function LanguageSwitch({ language, onChange }: { language: Language; onChange: (language: Language) => Promise<void> }) {
  return (
    <div className="language-switch">
      <Button size="sm" variant="ghost" className={language === 'en' ? 'active' : ''} onClick={() => { void onChange('en') }}>EN</Button>
      <Button size="sm" variant="ghost" className={language === 'zh-CN' ? 'active' : ''} onClick={() => { void onChange('zh-CN') }}>中文</Button>
    </div>
  )
}
