import type { ChartOptions } from 'chart.js';
import { readCssVar } from './theme';

/** Shared Chart.js scale / legend colours from CSS variables. */
export function chartThemeOptions(): Pick<ChartOptions, 'plugins' | 'scales'> {
  const text = readCssVar('--chart-text', '#6b7280');
  const grid = readCssVar('--chart-grid', '#e5e7eb');
  const legend = readCssVar('--chart-legend', '#374151');

  return {
    plugins: {
      legend: {
        labels: { color: legend },
      },
    },
    scales: {
      x: {
        ticks: { color: text },
        grid: { color: grid },
      },
      y: {
        ticks: { color: text, precision: 0 },
        grid: { color: grid },
        title: { color: text },
      },
    },
  };
}
