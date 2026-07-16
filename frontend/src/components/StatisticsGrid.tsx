import { useStatistics } from '../stores/repository-store';

export function StatisticsGrid() {
  const statistics = useStatistics();
  const items = [
    ['总数', statistics.total, 'total'],
    ['待更新', statistics.hasUpdates, 'danger'],
    ['本地修改', statistics.dirty, 'warning'],
    ['远程异常', statistics.unreachable, 'muted'],
  ] as const;

  return (
    <dl className="statistics-strip" aria-label="仓库统计">
      {items.map(([label, value, tone]) => (
        <div className={`stat-tag ${tone}`} key={label}>
          <dt>{label}</dt>
          <dd>{value.toLocaleString('zh-CN')}</dd>
        </div>
      ))}
    </dl>
  );
}
