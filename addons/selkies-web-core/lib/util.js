/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

export class Queue {
    /**
     * @constructor
     * @param {Array}
     *    Video element to attach events to
     */
    constructor(...elements) {
        /**
         * @type {Array}
         */
        this.items = [];

        this.enqueue(...elements);
    }

    enqueue(...elements) {
        elements.forEach(element => this.items.push(element));
    }

    dequeue(count=1) {
        return this.items.splice(0, count)[0];
    }

    size() {
        return this.items.length;
    }

    isEmpty() {
        return this.items.length===0;
    }

    toArray() {
        return [...this.items]
    }

    remove(element) {
        var index = this.items.indexOf(element)
        this.items.splice(index, 1)
    }

    find(element) {
        return this.items.indexOf(element) == -1 ? false: true;
    }

    clear(){
        this.items.length = 0;
    }
}
