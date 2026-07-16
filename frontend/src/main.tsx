import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './app/App';
import { DialogProvider } from './components/DialogProvider';
import './styles/global.css';

const root = document.getElementById('root');
if (root === null) throw new Error('缺少应用挂载节点');

createRoot(root).render(
  <StrictMode>
    <DialogProvider>
      <App />
    </DialogProvider>
  </StrictMode>,
);
