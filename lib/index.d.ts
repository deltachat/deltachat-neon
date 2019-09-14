/** Checks wether an email address could exist (Simple email format validation) */
export declare function maybeValidAddr(addr:string):boolean;

export declare class Context {
    constructor();
    open();
    configure();
    getInfo():{[key:string]:string};
    connect();
    close();
    isOpen():boolean;
    isConfigured():boolean;
    getConfig(key:string):string;
    getBlobdir():string;
}