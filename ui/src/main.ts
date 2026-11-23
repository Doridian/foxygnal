import { mount } from 'svelte';
import App from './App.svelte';
import Popout from './Popout.svelte';
import './app.css';

// `?popout=<id>` shows one instrument in its own window.
const popout = new URLSearchParams(location.search).get('popout');
const target = document.getElementById('app')!;
if (popout) mount(Popout, { target, props: { id: Number(popout) } });
else mount(App, { target });
