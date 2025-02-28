<!--
	SPDX-FileCopyrightText: 2021 Softbear, Inc.
	SPDX-License-Identifier: AGPL-3.0-or-later
-->

<script>
	import Meter from '../component/Meter.svelte';
	import t from '../util/translation.js';
	import {fly} from 'svelte/transition';
	import entityData from '../data/entities.json';
	import {cinematic, fpsShown} from '../util/settings.js';
	import {hasUpgrades, progressOfUpgrade, toKnotsString} from '../util/warship.js';

	export let state;
	$: alive = state.status.playing;
	$: progress = progressOfUpgrade(alive.type, state.score || 0);

	function directionString(angle) {
		// angle (0 to 1)
		const theta = (angle + Math.PI) / (2 * Math.PI);
		const directions = ['W', 'SW', 'S', 'SE', 'E', 'NE', 'N', 'NW'];
		const index = Math.round(theta * directions.length);
		return directions[((index + directions.length) % directions.length + directions.length) % directions.length];
	}

	function positionString(element, positiveLabel, negativeLabel) {
		return `${Math.round(Math.abs(element))}${element >= 0 ? positiveLabel : negativeLabel}`
	}

	function progressLabel(t, progress, nextLevel) {
		let basis = t('panel.upgrade.label.progress');

		return basis.replace("{percent}", `${Math.floor(progress * 100)}%`).replace("{level}", nextLevel);
	}
</script>

<div id="ship_status" class:cinematic={$cinematic} transition:fly="{{y: 100}}">
	<h2>
		{state.score || 0} {$t('panel.status.score' + (state.score === 1 ? '' : 'Plural'))} —
		{toKnotsString(alive.velocity)} —
		{Math.round(((Math.PI / 2 - alive.direction) * 180 / Math.PI % 360 + 360) % 360)}° [{directionString(alive.direction)}] —
		({positionString(alive.position.x, 'E', 'W')}, {positionString(alive.position.y, 'N', 'S')})
		{$fpsShown ? `— ${state.fps.toFixed(1)} fps` : ''}
	</h2>
	{#if hasUpgrades(alive.type)}
		<Meter value={progress}>{progressLabel($t, progress, entityData[alive.type].level + 1)}</Meter>
	{/if}
</div>

<style>
	div {
		text-align: center;
		user-select: none;
		transition: opacity 0.25s;
	}

	div.cinematic:not(:hover) {
		opacity: 0;
	}

	h2 {
		color: white;
		margin-bottom: 0.25em;
	}

	#ship_status {
		bottom: 0;
		left: 50%;
		max-width: 45%;
		padding-bottom: 1rem;
		position: absolute;
		transform: translate(-50%, 0);
	}
</style>
