import { RootProvider } from 'fumadocs-ui/provider/next';
import type { Metadata } from 'next';
import { Roboto, Roboto_Mono } from 'next/font/google';
import { siteDescription, siteName, siteUrl, withBasePath } from '@/lib/shared';
import './global.css';

const roboto = Roboto({ subsets: ['latin'], variable: '--font-roboto' });
const robotoMono = Roboto_Mono({ subsets: ['latin'], variable: '--font-roboto-mono' });

export const metadata: Metadata = {
  metadataBase: new URL(siteUrl),
  title: { template: `%s | ${siteName}`, default: siteName },
  description: siteDescription,
  icons: { icon: withBasePath('/assets/logo/favicon.ico') },
};

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html
      lang="en"
      className={`${roboto.variable} ${robotoMono.variable}`}
      suppressHydrationWarning
    >
      <body className="flex flex-col min-h-screen">
        <RootProvider
          search={{
            // A static export has no search server, so the index ships to the
            // browser and is queried there.
            options: { type: 'static', api: withBasePath('/api/search') },
          }}
        >
          {children}
        </RootProvider>
      </body>
    </html>
  );
}
