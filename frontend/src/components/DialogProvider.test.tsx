import { useState } from 'react';
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { DialogProvider, useDialog } from './DialogProvider';

function ConfirmHarness() {
  const dialog = useDialog();
  const [result, setResult] = useState('未操作');
  return (
    <>
      <button onClick={() => {
        void dialog.confirm({
          title: '确认备份更新',
          message: '测试自定义确认组件',
          confirmLabel: '开始更新',
          tone: 'warning',
        }).then((confirmed) => setResult(confirmed ? '已确认' : '已取消'));
      }}>
        打开确认
      </button>
      <output>{result}</output>
    </>
  );
}

function IssueHarness() {
  const dialog = useDialog();
  return (
    <button onClick={() => {
      void dialog.alert({
        title: '2 个仓库未获取远程状态',
        message: '失败仓库不会进入更新范围。',
        items: [{
          title: 'example',
          summary: '远程仓库不存在，或当前凭据没有访问权限。',
          context: 'needauth/example',
          technicalDetail: 'remote: Repository not found',
        }],
      });
    }}>
      查看问题
    </button>
  );
}

describe('自定义确认组件', () => {
  it('在应用内渲染并返回确认结果', async () => {
    render(<DialogProvider><ConfirmHarness /></DialogProvider>);

    fireEvent.click(screen.getByRole('button', { name: '打开确认' }));
    expect(screen.getByRole('dialog', { name: '确认备份更新' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '开始更新' }));

    expect(await screen.findByText('已确认')).toBeInTheDocument();
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('按 Escape 安全取消', async () => {
    render(<DialogProvider><ConfirmHarness /></DialogProvider>);

    fireEvent.click(screen.getByRole('button', { name: '打开确认' }));
    fireEvent.keyDown(document, { key: 'Escape' });

    expect(await screen.findByText('已取消')).toBeInTheDocument();
  });

  it('以摘要优先、技术详情折叠的形式展示仓库问题', () => {
    render(<DialogProvider><IssueHarness /></DialogProvider>);

    fireEvent.click(screen.getByRole('button', { name: '查看问题' }));

    expect(screen.getByText('远程仓库不存在，或当前凭据没有访问权限。')).toBeInTheDocument();
    expect(screen.getByText('needauth/example')).toBeInTheDocument();
    expect(screen.getByText('查看技术详情')).toBeInTheDocument();
  });
});
