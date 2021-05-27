/** Checks wether an email address could exist (Simple email format validation) */
export declare function maybeValidAddr(addr:string):boolean;

export declare class Context {
    constructor( callback: (event_type, data) => void); // TODO event type enum?
    /**
     * Opens the context
     * @param database Path to database file
     */
    open(database: string, callback: (err:Error)=>void):void;
    configure():void;
    getInfo():{[key:string]:string};
    connect():void;
    close():void;
    isOpen():boolean;
    isConfigured():boolean;
    getConfig(key:string):string;
    getBlobdir():string;
}